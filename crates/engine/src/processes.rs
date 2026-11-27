//! Pipe-backed commands owned by a single gateway session. Only the gateway
//! grants a session this registry. Dropping it stops pipe jobs and detaches PTY readers.
use anyhow::{Context, Result, bail, ensure};
use process_wrap::tokio::{CommandWrap, KillOnDrop};
mod controls;
pub use controls::ProcessControls;
mod pty;
mod receipt;
use pty::Input;
use receipt::Receipt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::watch,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 98_304;
const MAX_JOBS: usize = 64;
const MAX_SECONDS: u64 = 3_600;

pub(crate) fn specs() -> [Value; 2] {
    let spec = |name, description, properties, required| {
        json!({
            "type": "function", "function": {"name": name, "description": description,
            "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}
        })
    };
    [
        spec(
            "terminal",
            "Run a local Bash command. Foreground commands return their output, exit code and working directory. Background commands return a process session_id for process_manage.",
            json!({"command":{"type":"string"},"background":{"type":"boolean","default":false},
                "timeout":{"type":"integer","minimum":1,"maximum":MAX_SECONDS},"workdir":{"type":"string"},
                "pty":{"type":"boolean","default":false},
                "persist_on_release":{"type":"boolean","default":false,"description":"Keep a background job when its agent turn is abandoned; pipe jobs do not survive gateway exit."}}),
            json!(["command"]),
        ),
        spec(
            "process_manage",
            "Manage a process created by this conversation. Poll consumes new output, log reads the retained output, wait waits for exit, write sends raw text, submit adds a newline, close sends EOF, and kill terminates the managed process group.",
            json!({"action":{"enum":["list","poll","log","wait","write","submit","close","kill"]},
                "session_id":{"type":"string"},"data":{"type":"string"},"timeout":{"type":"integer","minimum":1,"maximum":MAX_SECONDS},
                "offset":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":2000}}),
            json!(["action"]),
        ),
    ]
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Launch {
    command: String,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    pty: bool,
    timeout: Option<u64>,
    workdir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    persist_on_release: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    notify_on_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notify: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch_patterns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    heartbeat_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_output_chars: Option<usize>,
}
fn is_false(value: &bool) -> bool { !*value }

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    List,
    Poll,
    Log,
    Wait,
    Write,
    Submit,
    Close,
    Kill,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manage {
    action: Action,
    session_id: Option<String>,
    data: Option<String>,
    timeout: Option<u64>,
    offset: Option<i64>,
    limit: Option<usize>,
}

pub(crate) enum Request {
    Launch(Launch),
    Manage(Manage),
}

impl Request {
    pub fn parse(name: &str, args: Value) -> Result<Self> {
        match name {
            "terminal" => {
                let a: Launch = serde_json::from_value(args)?;
                ensure!(
                    !a.command.trim().is_empty()
                        && a.command.len() <= 64 * 1024
                        && !a.command.contains('\0'),
                    "Invalid shell command"
                );
                ensure!(
                    a.timeout.is_none_or(|t| (1..=MAX_SECONDS).contains(&t)),
                    "Invalid timeout"
                );
                ensure!(!a.pty || a.background, "PTY requires background=true");
                ensure!(!a.persist_on_release || a.background, "persist_on_release requires a background job");
                ensure!(!a.notify_on_complete && !a.notify.unwrap_or(false) && a.watch_patterns.as_ref().is_none_or(Vec::is_empty) && a.heartbeat_seconds.is_none() && a.completion_output_chars.is_none(), "Background process notifications are not supported");
                ensure!(
                    a.workdir.as_ref().is_none_or(|p| p.is_absolute()),
                    "workdir must be absolute"
                );
                Ok(Self::Launch(a))
            }
            "process_manage" => {
                let a: Manage = serde_json::from_value(args)?;
                ensure!(
                    matches!(a.action, Action::List)
                        || a.session_id.as_ref().is_some_and(|id| !id.is_empty()),
                    "session_id is required"
                );
                ensure!(
                    a.data.as_ref().is_none_or(|s| s.len() <= 64 * 1024),
                    "Input too large"
                );
                ensure!(
                    !matches!(a.action, Action::Write | Action::Submit) || a.data.is_some(),
                    "data is required"
                );
                ensure!(
                    a.timeout.is_none_or(|t| (1..=MAX_SECONDS).contains(&t)),
                    "Invalid timeout"
                );
                ensure!(
                    a.limit.is_none_or(|n| (1..=2000).contains(&n)),
                    "Invalid log limit"
                );
                Ok(Self::Manage(a))
            }
            _ => bail!("Unknown process operation"),
        }
    }

    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Manage(Manage {
                action: Action::List | Action::Log | Action::Poll | Action::Wait,
                ..
            })
        )
    }
}

#[derive(Default)]
struct Log {
    chunks: VecDeque<(bool, Vec<u8>)>,
    bytes: usize,
    end: u64,
}

impl Log {
    fn push(&mut self, stderr: bool, bytes: &[u8]) {
        self.end += bytes.len() as u64;
        self.bytes += bytes.len();
        self.chunks.push_back((stderr, bytes.to_vec()));
        while self.bytes > MAX_OUTPUT {
            let (_, removed) = self.chunks.pop_front().unwrap();
            self.bytes -= removed.len();
        }
    }

    /// While the process runs, a character split across reads stays for the
    /// next poll so incremental output never shows a replacement character.
    fn snapshot(&self, from: u64, running: bool) -> (Value, u64) {
        let start = self.end - self.bytes as u64;
        let held = match self.chunks.back() {
            Some((_, last)) if running => incomplete_tail(last),
            _ => 0,
        };
        let end = self.end - held as u64;
        let mut skip = from.saturating_sub(start) as usize;
        let mut remaining = end.saturating_sub(from.max(start)) as usize;
        let (mut combined, mut stdout, mut stderr) = (Vec::new(), Vec::new(), Vec::new());
        for (is_error, bytes) in &self.chunks {
            if remaining == 0 {
                break;
            }
            if skip >= bytes.len() {
                skip -= bytes.len();
                continue;
            }
            let data = &bytes[skip..bytes.len().min(skip + remaining)];
            skip = 0;
            remaining -= data.len();
            combined.extend_from_slice(data);
            if *is_error {
                stderr.extend_from_slice(data);
            } else {
                stdout.extend_from_slice(data);
            }
        }
        (
            json!({"output":String::from_utf8_lossy(&combined),"stdout":String::from_utf8_lossy(&stdout),
            "stderr":String::from_utf8_lossy(&stderr),"truncated":from < start}),
            end.max(from),
        )
    }
}

/// Bytes at the end that begin a UTF-8 sequence the read has not finished.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0xF0.. => 4,
            0xE0.. => 3,
            0xC0.. => 2,
            _ => 1,
        };
        return if needed > back { back } else { 0 };
    }
    0
}

async fn drain(
    mut reader: impl AsyncRead + Unpin,
    log: Arc<Mutex<Log>>,
    stderr: bool,
) -> std::io::Result<()> {
    let mut block = [0; 4096];
    loop {
        let n = reader.read(&mut block).await?;
        if n == 0 {
            return Ok(());
        }
        log.lock().expect("process log").push(stderr, &block[..n]);
    }
}

#[derive(Clone)]
struct Exit {
    code: Option<i32>,
    error: Option<String>,
}

struct Job {
    id: String,
    pid: Option<u32>,
    stdin: Option<Input>,
    pty: Option<(crate::herdr::Core, crate::herdr::Terminal)>,
    stop: CancellationToken,
    done: watch::Receiver<Option<Exit>>,
    log: Arc<Mutex<Log>>,
    cursor: u64,
    turn: usize,
    persist_on_release: bool,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Job {
    fn spawn(mut command: CommandWrap, receipt: &Receipt, background: bool, turn: usize, persist_on_release: bool) -> Result<Self> {
        let id = receipt.id.clone();
        let mut child = command
            .spawn()
            .context("Could not launch terminal command")?;
        let pid = child.id();
        let stdin = child.stdin().take().map(Input::Pipe);
        let stdout = child.stdout().take().context("Missing child stdout pipe")?;
        let stderr = child.stderr().take().context("Missing child stderr pipe")?;
        let log = Arc::new(Mutex::new(Log::default()));
        let reader_log = log.clone();
        let (sender, done) = watch::channel(None);
        let stop = CancellationToken::new();
        let signal = stop.clone();
        let receipt = receipt.clone();
        tokio::spawn(async move {
            let stdout = drain(stdout, reader_log.clone(), false);
            let stderr = drain(stderr, reader_log.clone(), true);
            let drains = async { tokio::try_join!(stdout, stderr) };
            tokio::pin!(drains);
            let mut drained = false;
            let mut stopped = false;
            let result: Result<_> = async {
                // Continue to accept a stop while the shell's descendants hold
                // its pipes open. Publish completion after the final output.
                let status = loop {
                    tokio::select! {
                        status = child.wait() => break status?,
                        io = &mut drains, if !drained => { io?; drained = true; },
                        _ = signal.cancelled(), if !stopped => { child.start_kill()?; stopped = true; },
                    }
                };
                if !drained {
                    if !stopped {
                        tokio::select! {
                            io = &mut drains => { io?; drained = true; },
                            _ = signal.cancelled() => { let _ = child.start_kill(); }
                        }
                    }
                    if !drained { drains.await?; }
                }
                Ok(status.code())
            }.await;
            let mut exit = match result {
                Ok(code) => Exit { code, error: None },
                Err(error) => Exit {
                    code: None,
                    error: Some(error.to_string()),
                },
            };
            if background {
                let (mut saved, _) = reader_log.lock().expect("process log").snapshot(0, false);
                saved["session_id"] = json!(receipt.id);
                saved["status"] = json!("exited");
                saved["exit_code"] = json!(exit.code);
                saved["pid"] = json!(pid);
                if let Some(reason) = &exit.error {
                    saved["error"] = json!(reason);
                }
                if let Err(e) = receipt.save(&saved) {
                    exit.error = Some(format!("Could not commit process outcome: {e}"));
                }
            }
            sender.send_replace(Some(exit));
        });
        Ok(Self {
            id,
            pid,
            stdin,
            pty: None,
            stop,
            done,
            log,
            cursor: 0,
            turn,
            persist_on_release,
        })
    }

    async fn wait(&mut self, seconds: u64) -> Result<bool> {
        match timeout(
            Duration::from_secs(seconds),
            self.done.wait_for(|s| s.is_some()),
        )
        .await
        {
            Ok(result) => {
                result?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    fn result(&mut self, incremental: bool) -> Value {
        let start = if incremental { self.cursor } else { 0 };
        let running = self.done.borrow().is_none();
        let (mut data, end) = self.log.lock().expect("process log").snapshot(start, running);
        if incremental {
            self.cursor = end;
        }
        let exit = self.done.borrow();
        data["session_id"] = json!(self.id);
        data["pid"] = json!(self.pid);
        if self.pty.is_some() {
            data["terminal_id"] = json!(self.id);
        }
        data["status"] = json!(if exit.is_some() { "exited" } else { "running" });
        data["exit_code"] = json!(exit.as_ref().and_then(|v| v.code));
        if let Some(error) = exit.as_ref().and_then(|v| v.error.as_ref()) {
            data["error"] = json!(error);
        }
        data
    }
}

/// Owned by the gateway's engine session, scoped before a model can select a job.
pub(crate) struct Registry {
    jobs: VecDeque<Job>,
    controls: ProcessControls,
    directory: PathBuf,
    pty_owner: Option<(crate::herdr::Core, String)>,
    restored_polls: VecDeque<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

impl Registry {
    pub fn new(workspace: &Path, directory: &Path) -> Self {
        let safe_names = [
            "PATH",
            "HOME",
            "USER",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "TEMP",
            "TMP",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "TZ",
        ];
        let env = std::env::vars_os()
            .filter_map(|(k, v)| {
                let key = k.into_string().ok()?;
                let found = safe_names.iter().find(|name| {
                    if cfg!(windows) {
                        key.eq_ignore_ascii_case(name)
                    } else {
                        key == **name
                    }
                })?;
                Some(((*found).to_owned(), v.into_string().ok()?))
            })
            .collect();
        Self {
            jobs: VecDeque::new(),
            controls: ProcessControls::default(),
            directory: directory.into(),
            pty_owner: None,
            restored_polls: VecDeque::new(),
            cwd: workspace.into(),
            env,
        }
    }

    pub fn bind_terminals(&mut self, profile: &Path, conversation: &str) -> Result<()> {
        self.pty_owner = Some((crate::herdr::Core::new(profile)?, conversation.to_owned()));
        self.controls.owner.clone_from(&self.pty_owner);
        Ok(())
    }

    pub fn controls(&self) -> ProcessControls { self.controls.clone() }

    pub async fn invoke(&mut self, request: Request, turn: usize, call: &str) -> Result<Value> {
        match request {
            Request::Launch(a) => self.launch(a, turn, call).await,
            Request::Manage(a) => self.manage(a).await,
        }
    }

    async fn launch(&mut self, a: Launch, turn: usize, call: &str) -> Result<Value> {
        let cwd = a
            .workdir
            .as_deref()
            .unwrap_or(&self.cwd)
            .canonicalize()
            .context("Invalid working directory")?;
        ensure!(cwd.is_dir(), "Working directory is not a directory");
        if a.background && self.jobs.len() >= MAX_JOBS {
            let i = self
                .jobs
                .iter()
                .position(|job| job.done.borrow().is_some())
                .context("Maximum running processes reached")?;
            self.jobs.remove(i);
        }
        let shell = shell()?;
        if a.pty {
            let (core, conversation) = self
                .pty_owner
                .as_ref()
                .context("Interactive terminal service unavailable")?;
            let key = format!("{turn}:{call}");
            let args = vec![
                shell.to_string_lossy().into_owned(),
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                a.command,
            ];
            let id = core
                .launch_agent_pty(conversation, &key, &cwd, &args)
                .await?;
            let controls = self.controls.clone();
            let job = self.pty_job(&id).await?;
            job.turn = turn;
            job.persist_on_release = a.persist_on_release;
            controls.register(job, &cwd, args.last().context("Missing command")?);
            return Ok(job.result(false));
        }
        let state = if a.background {
            None
        } else {
            Some(tempfile::NamedTempFile::new()?)
        };
        let mut command = CommandWrap::with_new(&shell, |cmd| {
            cmd.env_clear()
                .envs(&self.env)
                .current_dir(&cwd)
                .stdin(if a.background {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.args(["--noprofile", "--norc", "-c"]);
            if let Some(file) = &state {
                cmd.arg(FOREGROUND)
                    .arg("hermes")
                    .arg(&a.command)
                    .arg(bash_path(file.path()));
            } else {
                cmd.arg(&a.command);
            }
        });
        wrap_process_group(&mut command);
        let (receipt, fresh) = Receipt::claim(&self.directory, turn, call, &a)?;
        if !fresh {
            if let Some(job) = self.jobs.iter_mut().find(|j| j.id == receipt.id) {
                return Ok(job.result(false));
            }
            return Ok(receipt.read()?.unwrap_or_else(|| receipt.unknown()));
        }
        let mut job = match Job::spawn(command, &receipt, a.background, turn, a.persist_on_release) {
            Ok(job) => job,
            Err(error) => {
                receipt.save(&json!({"session_id":receipt.id,"status":"failed","error":format!("{error:#}")}))?;
                return Err(error);
            }
        };
        if a.background {
            let result = json!({"session_id":job.id,"pid":job.pid,"status":"running","cwd":cwd});
            self.controls.register(&job, &cwd, &a.command);
            self.jobs.push_back(job);
            return Ok(result);
        }
        let finished = job.wait(a.timeout.unwrap_or(180)).await?;
        if !finished {
            job.stop.cancel();
            ensure!(
                job.wait(10).await?,
                "Timed-out command is still terminating"
            );
        }
        let mut result = job.result(false);
        if !finished {
            result["timed_out"] = json!(true);
        }
        if finished && let Some(file) = state {
            if let Some((cwd_after, env)) = load_state(file)? {
                self.env = env;
                if a.workdir.is_none() {
                    self.cwd = cwd_after;
                }
            }
        }
        result["cwd"] = json!(if a.workdir.is_some() { &cwd } else { &self.cwd });
        receipt.save(&result)?;
        Ok(result)
    }

    async fn pty_job(&mut self, id: &str) -> Result<&mut Job> {
        if let Some(i) = self.jobs.iter().position(|j| j.id == id) {
            return Ok(&mut self.jobs[i]);
        }
        let (core, owner) = self
            .pty_owner
            .as_ref()
            .context("Interactive terminal service unavailable")?;
        let terminal = core
            .lookup(id)?
            .filter(|t| t.conversation_id.as_ref() == Some(owner))
            .context("Process not found in this conversation")?;
        let job = Job::attach(core.clone(), terminal).await?;
        if self.jobs.len() >= MAX_JOBS {
            let i = self
                .jobs
                .iter()
                .position(|j| j.done.borrow().is_some())
                .context("Maximum running processes reached")?;
            self.jobs.remove(i);
        }
        self.jobs.push_back(job);
        Ok(self.jobs.back_mut().unwrap())
    }

    async fn manage(&mut self, a: Manage) -> Result<Value> {
        if matches!(a.action, Action::List) {
            return Ok(
                json!({"processes":self.jobs.iter().map(|j|json!({"session_id":j.id,"pid":j.pid,"status":if j.done.borrow().is_some() {"exited"} else {"running"}})).collect::<Vec<_>>()}),
            );
        }
        let id = a.session_id.as_deref().context("session_id is required")?;
        if crate::herdr::valid_id(id) {
            let job = self.pty_job(id).await?;
            return Self::manage_job(job, a).await;
        }
        let token = id.strip_prefix("proc_").unwrap_or(id);
        ensure!(!token.is_empty(), "Empty process ID");
        let mut matches = self.jobs.iter_mut().filter(|job| {
            job.id
                .strip_prefix("proc_")
                .is_some_and(|s| s.starts_with(token))
        });
        let Some(job) = matches.next() else {
            if matches!(a.action, Action::Wait | Action::Log | Action::Poll)
                && let Ok(receipt) = Receipt::for_id(&self.directory, id)
                && let Some(mut saved) = receipt.read()?
            {
                if matches!(a.action, Action::Log) {
                    return Ok(log_page(&saved, &a));
                }
                if self.restored_polls.iter().any(|previous| previous == id) {
                    for key in ["output", "stdout", "stderr"] {
                        saved[key] = json!("");
                    }
                } else {
                    self.restored_polls.push_back(id.to_owned());
                    if self.restored_polls.len() > MAX_JOBS {
                        self.restored_polls.pop_front();
                    }
                }
                return Ok(saved);
            }
            bail!("Process not found in this conversation");
        };
        ensure!(matches.next().is_none(), "Ambiguous process ID");
        Self::manage_job(job, a).await
    }

    async fn manage_job(job: &mut Job, a: Manage) -> Result<Value> {
        match a.action {
            Action::List => unreachable!(),
            Action::Poll => Ok(job.result(true)),
            Action::Log => Ok(log_page(&job.result(false), &a)),
            Action::Wait => {
                let finished = job.wait(a.timeout.unwrap_or(30)).await?;
                let mut result = job.result(true);
                result["timed_out"] = json!(!finished);
                Ok(result)
            }
            Action::Kill => {
                if job.done.borrow().is_none() {
                    if let Some((core, t)) = &job.pty {
                        core.kill(t).await?;
                    } else {
                        job.stop.cancel();
                    }
                    ensure!(job.wait(10).await?, "Process termination pending");
                }
                Ok(job.result(true))
            }
            Action::Close => {
                if let Some(Input::Pty(input)) = job.stdin.as_mut() {
                    input.lock().await.send(&[4]).await?;
                    job.stdin.take();
                } else {
                    job.stdin.take();
                }
                Ok(json!({"session_id":job.id,"status":"stdin_closed"}))
            }
            Action::Write | Action::Submit => {
                let input = job.stdin.as_mut().context("Process stdin is closed")?;
                let mut bytes = a.data.unwrap_or_default().into_bytes();
                if matches!(a.action, Action::Submit) {
                    bytes.push(if job.pty.is_some() { b'\r' } else { b'\n' });
                }
                timeout(Duration::from_secs(5), input.write(&bytes))
                    .await
                    .context(
                        "Timed out writing stdin; input may have been partially delivered",
                    )??;
                Ok(json!({"session_id":job.id,"bytes_written":bytes.len()}))
            }
        }
    }
}

fn log_page(data: &Value, a: &Manage) -> Value {
    let text = data["output"].as_str().unwrap_or_default();
    let lines: Vec<_> = text.lines().collect();
    let limit = a.limit.unwrap_or(200);
    let offset = a
        .offset
        .map(|n| {
            if n < 0 {
                lines.len().saturating_sub(n.unsigned_abs() as usize)
            } else {
                n as usize
            }
        })
        .unwrap_or_else(|| lines.len().saturating_sub(limit));
    let output = lines
        .iter()
        .skip(offset)
        .take(limit)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    json!({"session_id":data["session_id"],"status":data["status"],"exit_code":data["exit_code"],
        "output":output,"offset":offset,"total_lines":lines.len(),"truncated":data["truncated"]})
}

pub(crate) fn wrap_process_group(command: &mut CommandWrap) {
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command
        .wrap(process_wrap::tokio::CreationFlags(
            windows::Win32::System::Threading::CREATE_NO_WINDOW,
        ))
        .wrap(process_wrap::tokio::JobObject);
}

#[cfg(windows)]
pub(crate) fn shell() -> Result<PathBuf> {
    for root in [
        "ProgramW6432",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
    ] {
        if let Some(root) = std::env::var_os(root) {
            for name in ["Git/bin/bash.exe", "Programs/Git/bin/bash.exe"] {
                let candidate = Path::new(&root).join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    bail!("Local agent commands require Git for Windows Bash")
}

#[cfg(unix)]
pub(crate) fn shell() -> Result<PathBuf> {
    ["/bin/bash", "/usr/bin/bash"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .context("Local agent commands require Bash")
}

fn bash_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        s.trim_start_matches(r"\\?\").replace('\\', "/")
    } else {
        s.into_owned()
    }
}

// Pass the user command and state path as separate argv entries. Snapshot only
// data (cwd and exported variables). Never source a previous command's output.
const FOREGROUND: &str = r#"readonly __hermes_state="$2"
__hermes_save() {
    {
        if [[ -n ${MSYSTEM-} || $OSTYPE == msys* ]]; then
            printf '%s\0' "$(pwd -W)"
        else
            printf '%s\0' "$(pwd -P)"
        fi
        while IFS= read -r name; do
            case "$name" in
                PWD|OLDPWD|SHLVL|_) continue ;;
                PATH) if [[ $OSTYPE == msys* ]]; then printf '%s=%s\0' PATH "$(/usr/bin/cygpath -wp "$PATH")"; continue; fi ;;
            esac
            printf '%s=%s\0' "$name" "${!name}"
        done < <(compgen -e)
    } > "$__hermes_state"
}
trap '__hermes_save' EXIT
eval "$1"
"#;

fn load_state(
    file: tempfile::NamedTempFile,
) -> Result<Option<(PathBuf, BTreeMap<String, String>)>> {
    let mut raw = Vec::new();
    file.reopen()?.take(1_048_577).read_to_end(&mut raw)?;
    if raw.is_empty() || raw.len() > 1_048_576 || raw.last() != Some(&0) {
        return Ok(None);
    }
    let mut fields = raw.split(|&b| b == 0);
    let cwd = PathBuf::from(String::from_utf8_lossy(fields.next().unwrap_or_default()).as_ref());
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Ok(None);
    }
    let mut env = BTreeMap::new();
    for field in fields {
        let Ok(pair) = std::str::from_utf8(field) else {
            continue;
        };
        if let Some((key, value)) = pair.split_once('=')
            && !key.is_empty()
        {
            env.insert(key.to_owned(), value.to_owned());
        }
    }
    Ok(Some((cwd, env)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_output_never_splits_a_character_between_polls() {
        let mut log = Log::default();
        let text = "héllo 😀".as_bytes();
        let split = text.len() - 2;
        log.push(false, &text[..split]);
        let (first, cursor) = log.snapshot(0, true);
        log.push(false, &text[split..]);
        let (second, _) = log.snapshot(cursor, true);
        let joined = format!(
            "{}{}",
            first["output"].as_str().unwrap(),
            second["output"].as_str().unwrap()
        );
        assert_eq!(joined, "héllo 😀");
        // A finished process reports every byte it wrote.
        let mut ended = Log::default();
        ended.push(false, &text[..split]);
        assert_eq!(ended.snapshot(0, false).1, split as u64);
    }

    async fn call(r: &mut Registry, name: &str, args: Value) -> Result<Value> {
        static CALL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = CALL
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string();
        r.invoke(Request::parse(name, args)?, 0, &id).await
    }

    #[test]
    fn invalid_persistence_and_notifications_fail_closed() {
        assert!(Request::parse("terminal", json!({"command":"sleep 1","persist_on_release":true})).is_err());
        assert!(Request::parse("terminal", json!({"command":"sleep 1","background":true,"persist_on_release":true})).is_ok());
        assert!(Request::parse("terminal", json!({"command":"sleep 1","background":true,"notify_on_complete":true})).is_err());
        assert!(Request::parse("terminal", json!({"command":"sleep 1","background":true,"pty":true,"persist_on_release":true})).is_ok());
    }

    #[tokio::test]
    async fn abandoned_turn_releases_only_its_nonpersistent_jobs() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let mut registry = Registry::new(workspace.path(), workspace.path());
        let launch = || Request::parse("terminal", json!({"command":"read -r forever","background":true}));
        let earlier = registry.invoke(launch()?, 1, "earlier").await?["session_id"].as_str().unwrap().to_owned();
        let abandoned = registry.invoke(launch()?, 2, "abandoned").await?["session_id"].as_str().unwrap().to_owned();
        let persistent = registry.invoke(Request::parse("terminal", json!({"command":"read -r forever","background":true,"persist_on_release":true}))?, 2, "persistent").await?["session_id"].as_str().unwrap().to_owned();
        let controls = registry.controls();
        drop(controls.clone()); // Releasing a viewer/control handle is not agent release.
        assert_eq!(controls.release_turn(2).await?, 1);
        let rows = controls.list().await?;
        assert_eq!(rows.iter().find(|row| row["session_id"] == abandoned).unwrap()["status"], "exited");
        assert_eq!(rows.iter().find(|row| row["session_id"] == earlier).unwrap()["status"], "running");
        assert_eq!(rows.iter().find(|row| row["session_id"] == persistent).unwrap()["status"], "running");
        assert_eq!(controls.stop().await?, 2, "explicit stop includes a persistent job");
        Ok(())
    }

    #[tokio::test]
    async fn foreground_preserves_state_and_reports_streams_and_nonzero_exit() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        std::fs::create_dir(workspace.path().join("next"))?;
        let mut r = Registry::new(workspace.path(), workspace.path());
        let one=call(&mut r,"terminal",json!({"command":"cd next && export HGR_TEST_STATE=remembered; printf first; printf second >&2; exit 7"})).await?;
        assert_eq!(one["exit_code"], 7, "{one}");
        assert_eq!(one["stdout"], "first");
        assert_eq!(one["stderr"], "second");
        assert_eq!(
            Path::new(one["cwd"].as_str().unwrap()).file_name().unwrap(),
            "next"
        );
        let two = call(
            &mut r,
            "terminal",
            json!({"command":r#"printf '%s:%s' "$HGR_TEST_STATE" "${PWD##*/}""#}),
        )
        .await?;
        assert_eq!(two["stdout"], "remembered:next", "{two}");
        Ok(())
    }

    #[tokio::test]
    async fn background_is_private_pipe_with_incremental_output_and_eof() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let mut owner = Registry::new(workspace.path(), workspace.path());
        let other_session = tempfile::tempdir()?;
        let mut foreign = Registry::new(workspace.path(), other_session.path());
        let started=call(&mut owner,"terminal",json!({"background":true,"command":r#"[[ ! -t 0 ]] || exit 8; printf 'ready\n'; while IFS= read -r line; do printf '<%s>\n' "$line"; done; printf 'eof' >&2; exit 3"#})).await?;
        let id = started["session_id"].as_str().unwrap();
        assert!(
            call(
                &mut foreign,
                "process_manage",
                json!({"action":"poll","session_id":id})
            )
            .await
            .is_err()
        );
        let mut ready = String::new();
        timeout(Duration::from_secs(15), async {
            while !ready.contains("ready") {
                let poll = call(
                    &mut owner,
                    "process_manage",
                    json!({"action":"poll","session_id":id}),
                )
                .await?;
                ready.push_str(poll["output"].as_str().unwrap());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        call(
            &mut owner,
            "process_manage",
            json!({"action":"write","session_id":id,"data":"foo"}),
        )
        .await?;
        call(
            &mut owner,
            "process_manage",
            json!({"action":"submit","session_id":id,"data":"bar"}),
        )
        .await?;
        call(
            &mut owner,
            "process_manage",
            json!({"action":"close","session_id":id}),
        )
        .await?;
        let result = call(
            &mut owner,
            "process_manage",
            json!({"action":"wait","session_id":id,"timeout":15}),
        )
        .await?;
        assert_eq!(result["exit_code"], 3, "{result}");
        assert_eq!(result["stdout"], "<foobar>\n");
        assert_eq!(result["stderr"], "eof");
        let poll = call(
            &mut owner,
            "process_manage",
            json!({"action":"poll","session_id":id}),
        )
        .await?;
        assert_eq!(poll["output"], "");
        Ok(())
    }

    #[tokio::test]
    async fn kill_ends_a_waiting_process() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let mut r = Registry::new(workspace.path(), workspace.path());
        let job = call(
            &mut r,
            "terminal",
            json!({"command":"read -r forever","background":true}),
        )
        .await?;
        let killed = call(
            &mut r,
            "process_manage",
            json!({"action":"kill","session_id":job["session_id"]}),
        )
        .await?;
        assert_eq!(killed["status"], "exited");
        assert_ne!(killed["exit_code"], 0);
        Ok(())
    }
    #[tokio::test]
    async fn retry_uses_durable_claim_and_completed_receipt() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let args = json!({"command":"printf once >> effect; printf job-output","background":true});
        let mut owner = Registry::new(dir.path(), dir.path());
        let started = owner
            .invoke(Request::parse("terminal", args.clone())?, 3, "call-A")
            .await?;
        let id = started["session_id"].as_str().unwrap();
        let done = call(
            &mut owner,
            "process_manage",
            json!({"action":"wait","session_id":id,"timeout":15}),
        )
        .await?;
        assert_eq!(done["exit_code"], 0, "{done}");
        drop(owner);
        let mut restarted = Registry::new(dir.path(), dir.path());
        let replay = restarted
            .invoke(Request::parse("terminal", args.clone())?, 3, "call-A")
            .await?;
        assert_eq!(replay["session_id"], id);
        assert_eq!(replay["output"], "job-output");
        assert_eq!(std::fs::read(dir.path().join("effect"))?, b"once");
        let conflict = Request::parse(
            "terminal",
            json!({"command":"echo other","background":true}),
        )?;
        assert!(restarted.invoke(conflict, 3, "call-A").await.is_err());
        std::fs::remove_file(
            dir.path()
                .join("process-results")
                .join(format!("{id}.json")),
        )?;
        let unknown = restarted
            .invoke(Request::parse("terminal", args)?, 3, "call-A")
            .await?;
        assert_eq!(unknown["status"], "unknown");
        assert_eq!(std::fs::read(dir.path().join("effect"))?, b"once");
        Ok(())
    }
}
