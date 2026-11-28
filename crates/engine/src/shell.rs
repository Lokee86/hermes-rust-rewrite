//! Captured local composer commands.
use crate::processes;
use process_wrap::tokio::CommandWrap;
use serde::Serialize;
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

mod environment;
#[cfg(windows)]
mod findstr;
mod policy;
#[cfg(windows)]
mod single_process;

const DEADLINE: Duration = Duration::from_secs(30);

pub struct Command {
    script: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Unsupported,
    CwdUnavailable,
    Timeout,
    Failed,
}

#[derive(Serialize)]
pub struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}

struct Prepared {
    command: CommandWrap,
    input: Option<Vec<u8>>,
    #[cfg(windows)]
    _file_list: Option<findstr::FileList>,
}

impl Prepared {
    fn new(command: CommandWrap) -> Self {
        Self {
            command,
            input: None,
            #[cfg(windows)]
            _file_list: None,
        }
    }
}

impl Command {
    pub fn parse(input: &str) -> Result<Self, Error> {
        if input.trim().is_empty() || input.contains('\0') {
            return Err(Error::Invalid);
        }
        Ok(Self {
            script: input.to_owned(),
        })
    }

    pub async fn capture(
        self,
        cwd: &Path,
        stop: CancellationToken,
        protected_values: &[&str],
    ) -> Result<Output, Error> {
        self.capture_for(cwd, stop, protected_values, DEADLINE)
            .await
    }

    /// Check the native capture policy and launch directory without spawning a process.
    pub fn check(self, cwd: &Path) -> Result<(), Error> {
        self.prepare(cwd, false).map(drop)
    }

    fn prepare(self, cwd: &Path, _launch: bool) -> Result<Prepared, Error> {
        #[cfg(unix)]
        let shell = std::path::PathBuf::from("/bin/sh");
        #[cfg(windows)]
        let shell = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::PathBuf::from(root)
                    .join("System32")
                    .join("cmd.exe")
            })
            .ok_or(Error::Failed)?;

        #[cfg(windows)]
        let more = shell.with_file_name("more.com");
        // Windows ships MORE as a COM executable. Launch that single validated
        // command directly so the native shell can retain its EXE-only PATH.
        #[cfg(windows)]
        let more_operand =
            policy::more_operand(&self.script).filter(|_| more.is_absolute() && more.is_file());
        #[cfg(windows)]
        let direct = more_operand.is_some();
        #[cfg(not(windows))]
        let direct = false;

        let environment = environment::snapshot(cwd, &shell, &self.script);
        let programs: Vec<&str> = environment.programs.iter().map(String::as_str).collect();
        let context = policy::Context {
            cwd,
            environment: &environment.values,
            trusted_programs: &programs,
        };
        #[cfg(windows)]
        let findstr = policy::findstr_search(&self.script);
        #[cfg(windows)]
        let scoped_search = findstr.is_some();
        #[cfg(not(windows))]
        let scoped_search = false;
        let git = policy::git_query(&self.script, &context);
        #[cfg(windows)]
        let echo_findstr = policy::echo_findstr(&self.script, &context);
        #[cfg(not(windows))]
        let echo_findstr: Option<(&str, &str, &str)> = None;
        let pip = policy::pip_query(&self.script, &context);
        if git.is_none()
            && pip.is_none()
            && !direct
            && !scoped_search
            && echo_findstr.is_none()
            && !policy::allows_with_context(&self.script, &context)
        {
            return Err(Error::Unsupported);
        }
        if !cwd.is_absolute() || !cwd.is_dir() || cwd.canonicalize().ok().as_deref() != Some(cwd) {
            return Err(Error::CwdUnavailable);
        }
        #[cfg(windows)]
        if let Some(search) = findstr {
            let file_list = _launch
                .then(|| findstr::FileList::build(cwd, &search))
                .transpose()?;
            let executable = shell.with_file_name("findstr.exe");
            if !executable.is_absolute() || !executable.is_file() {
                return Err(Error::Unsupported);
            }
            let args = file_list
                .as_ref()
                .map(|list| list.arguments(search))
                .transpose()?
                .unwrap_or_default();
            let mut command = CommandWrap::with_new(executable, |cmd| {
                cmd.env_clear()
                    .envs(environment.values)
                    .current_dir(file_list.as_ref().map_or(cwd, |list| list.directory(cwd)))
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .args(args);
                cmd.creation_flags(0x0800_0000);
            });
            processes::wrap_process_group(&mut command);
            return Ok(Prepared {
                command,
                input: None,
                _file_list: file_list,
            });
        }
        if let Some((program, args)) = git {
            #[cfg(windows)]
            let worktree_read = policy::worktree_read(&args);
            #[cfg(windows)]
            let program = if worktree_read {
                single_process::git_executable(&program, cwd).ok_or(Error::Unsupported)?
            } else {
                program
            };
            let mut command = CommandWrap::with_new(program, |cmd| {
                cmd.env_clear();
                // Git must not inherit repository overrides or credentials.
                cmd.envs(
                    environment
                        .values
                        .iter()
                        .filter(|(key, _)| {
                            ["SystemRoot", "WINDIR", "TMP", "TEMP", "TMPDIR"]
                                .iter()
                                .any(|name| key.eq_ignore_ascii_case(name))
                        })
                        .map(|(key, value)| (key, value)),
                );
                #[cfg(windows)]
                if worktree_read {
                    cmd.env("GIT_WORK_TREE", single_process::worktree(cwd));
                }
                cmd.env("LC_ALL", "C")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env(
                        "GIT_CONFIG_GLOBAL",
                        if cfg!(windows) { "NUL" } else { "/dev/null" },
                    )
                    .env("GIT_NO_LAZY_FETCH", "1")
                    .env("GIT_ALLOW_PROTOCOL", "")
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .current_dir(cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .args([
                        "--no-pager",
                        "--no-optional-locks",
                        "--literal-pathspecs",
                        "-c",
                        "core.fsmonitor=false",
                        "-c",
                        "safe.bareRepository=explicit",
                        "-c",
                        "core.quotePath=true",
                    ])
                    .arg(args[0]);
                if args[0] == "log" {
                    cmd.args([
                        "--no-show-signature",
                        "--no-notes",
                        "--no-patch",
                        "--pretty=medium",
                    ]);
                } else if args[0] == "status" {
                    cmd.arg("--ignore-submodules=all");
                } else {
                    cmd.args(["--no-ext-diff", "--no-textconv", "--ignore-submodules=all"]);
                }
                cmd.args(&args[1..]);
                #[cfg(windows)]
                cmd.creation_flags(0x0800_0000);
            });
            processes::wrap_process_group(&mut command);
            #[cfg(windows)]
            if worktree_read {
                command.wrap(single_process::Limit::default());
            }
            return Ok(Prepared::new(command));
        }
        #[cfg(windows)]
        if let Some((literal, trailing_spaces, pattern)) = echo_findstr {
            let executable = shell
                .with_file_name("findstr.exe")
                .canonicalize()
                .ok()
                .filter(|program| program.is_file() && !program.starts_with(cwd))
                .ok_or(Error::Unsupported)?;
            let mut input = Vec::with_capacity(literal.len() + 4);
            input.push(b'"');
            input.extend_from_slice(literal.as_bytes());
            input.push(b'"');
            input.extend_from_slice(trailing_spaces.as_bytes());
            input.extend_from_slice(b"\r\n");
            let mut command = CommandWrap::with_new(&executable, |cmd| {
                cmd.env_clear()
                    .envs(environment.values)
                    .current_dir(cwd)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .arg(pattern);
                #[cfg(windows)]
                cmd.creation_flags(0x0800_0000);
            });
            processes::wrap_process_group(&mut command);
            return Ok(Prepared {
                input: Some(input),
                ..Prepared::new(command)
            });
        }
        #[cfg(windows)]
        let executable = if direct { &more } else { &shell };
        #[cfg(not(windows))]
        let executable = &shell;
        let executable = pip.as_ref().unwrap_or(executable);
        let mut command = CommandWrap::with_new(executable, |cmd| {
            cmd.env_clear().envs(environment.values);
            cmd.current_dir(cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if pip.is_some() {
                // Ignore workspace and user-site modules and all PYTHON* overrides.
                cmd.args(["-I", "-m", "pip", "--version"]);
            } else {
                #[cfg(unix)]
                cmd.arg("-c").arg(&self.script);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    if let Some(file) = more_operand {
                        cmd.arg(file);
                    } else {
                        cmd.args(["/d", "/v:off", "/s", "/c"]);
                        // cmd.exe parses its command line itself; retain the outer quotes
                        // used by Python's shell=True and leave the script verbatim.
                        cmd.as_std_mut().raw_arg(format!("\"{}\"", self.script));
                    }
                }
            }
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        });
        processes::wrap_process_group(&mut command);
        Ok(Prepared::new(command))
    }

    async fn capture_for(
        self,
        cwd: &Path,
        stop: CancellationToken,
        protected_values: &[&str],
        deadline: Duration,
    ) -> Result<Output, Error> {
        let cwd = cwd.to_owned();
        let mut prepared = tokio::task::spawn_blocking(move || self.prepare(&cwd, true))
            .await
            .map_err(|_| Error::Failed)??;
        if stop.is_cancelled() {
            return Err(Error::Failed);
        }
        let mut child = prepared.command.spawn().map_err(|_| Error::Failed)?;
        let input = prepared.input.take();
        let stdin = input
            .as_ref()
            .map(|_| child.stdin().take().ok_or(Error::Failed))
            .transpose()?;
        let stdout = child.stdout().take().ok_or(Error::Failed)?;
        let stderr = child.stderr().take().ok_or(Error::Failed)?;
        let write_input = async move {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(&input).await?;
            }
            Ok::<(), io::Error>(())
        };
        let captured = {
            let run = async {
                let (stdout, stderr, status, ()) =
                    tokio::try_join!(read(stdout), read(stderr), child.wait(), write_input,)?;
                Ok::<_, io::Error>((stdout, stderr, status))
            };
            tokio::select! {
                biased;
                _ = stop.cancelled() => Err(Error::Failed),
                result = tokio::time::timeout(deadline, run) => result
                    .map_err(|_| Error::Timeout)
                    .and_then(|result| result.map_err(|_| Error::Failed)),
            }
        };
        let (stdout, stderr, status) = match captured {
            Ok(captured) => captured,
            Err(error) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                return Err(error);
            }
        };
        let code = status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                -status.signal().unwrap_or(1)
            }
            #[cfg(not(unix))]
            -1
        });
        Ok(Output {
            stdout: output_tail(&stdout, protected_values, 4000),
            stderr: output_tail(&stderr, protected_values, 2000),
            code,
        })
    }
}

/// Far more than the few thousand characters returned, so redaction still sees whole lines.
const RETAINED: usize = 1024 * 1024;

// Retain a generous tail until redaction, not the whole stream: a large log must not exhaust
// memory. A credential cut by the start of the tail would escape redaction, so once anything was
// dropped the partial first line goes too.
async fn read(mut reader: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    let mut dropped = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 { break; }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > 2 * RETAINED {
            output.drain(..output.len() - RETAINED);
            dropped = true;
        }
    }
    if dropped {
        if output.len() > RETAINED { output.drain(..output.len() - RETAINED); }
        let line = output.iter().position(|byte| *byte == b'\n').map_or(output.len(), |end| end + 1);
        output.drain(..line);
    }
    Ok(output)
}

fn output_tail(bytes: &[u8], protected: &[&str], limit: usize) -> String {
    // Python text mode decodes with replacement and translates CRLF and CR.
    let decoded = String::from_utf8_lossy(bytes);
    let normalized = decoded.replace("\r\n", "\n").replace('\r', "\n");
    let redacted = environment::redact(&normalized, protected);
    let start = redacted
        .char_indices()
        .rev()
        .nth(limit)
        .map_or(0, |(i, c)| i + c.len_utf8());
    redacted[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_large_stream_keeps_a_bounded_tail_of_whole_lines() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"token=start-of-a-secret-that-spans");
        while stream.len() < 5 * RETAINED { stream.extend_from_slice(b"log line\n"); }
        stream.extend_from_slice(b"last line\n");
        let kept = read(stream.as_slice()).await.unwrap();
        assert!(kept.len() <= RETAINED, "{}", kept.len());
        assert!(kept.starts_with(b"log line\n"), "the cut first line is dropped");
        assert!(kept.ends_with(b"last line\n"));
        let small = read(&b"short output"[..]).await.unwrap();
        assert_eq!(small, b"short output", "a stream that fits is kept whole");
    }

    #[test]
    fn rejects_empty_and_nul_input() {
        assert!(matches!(Command::parse("  "), Err(Error::Invalid)));
        assert!(matches!(Command::parse("echo a\0b"), Err(Error::Invalid)));
    }

    #[test]
    fn normalizes_newlines_and_invalid_utf8() {
        assert_eq!(output_tail(b"a\r\nb\rc\xff", &[], 100), "a\nb\nc\u{fffd}");
    }

    #[test]
    fn takes_unicode_character_tail_after_redaction() {
        let secret = "my-protected-secret";
        let text = format!("{}{}{}", "a".repeat(270_000), secret, "é".repeat(3990));
        let tail = output_tail(text.as_bytes(), &[secret], 4000);
        assert!(!tail.contains(secret));
        assert!(!tail.contains("protected-secret"));
        assert!(tail.ends_with(&"é".repeat(3990)));
        assert!(tail.chars().count() <= 4000);
    }

    #[test]
    fn redacts_url_credentials_before_tail() {
        let text = format!(
            "{}https://user:password@example.com/{}",
            "a".repeat(270_000),
            "z".repeat(3980)
        );
        let tail = output_tail(text.as_bytes(), &[], 4000);
        assert!(!tail.contains("password"));
        assert!(!tail.contains("user:"));
        assert!(tail.ends_with(&"z".repeat(3980)));
    }

    #[test]
    fn captures_isolated_pip_version() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let script = "python -m pip --version";
        #[cfg(unix)]
        let shell = std::path::PathBuf::from("/bin/sh");
        #[cfg(windows)]
        let shell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        let environment = environment::snapshot(&cwd, &shell, script);
        let programs: Vec<_> = environment.programs.iter().map(String::as_str).collect();
        let context = policy::Context {
            cwd: &cwd,
            environment: &environment.values,
            trusted_programs: &programs,
        };
        let Some(python) = policy::pip_query(script, &context) else {
            assert!(
                !programs.contains(&"python"),
                "installed python was not resolved"
            );
            return;
        };
        std::fs::write(cwd.join("pip.py"), "print('workspace module loaded')").unwrap();
        let expected = std::process::Command::new(python)
            .current_dir(&cwd)
            .args(["-I", "-m", "pip", "--version"])
            .output()
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let actual = runtime
            .block_on(
                Command::parse(script)
                    .unwrap()
                    .capture(&cwd, CancellationToken::new(), &[]),
            )
            .unwrap();
        assert_eq!(actual.code, expected.status.code().unwrap());
        assert_eq!(actual.stdout, output_tail(&expected.stdout, &[], 64_000));
        assert_eq!(actual.stderr, output_tail(&expected.stderr, &[], 64_000));
        assert!(!actual.stdout.contains("workspace module loaded"));
        for command in ["python -m other --version", "python -m pip install fixture"] {
            assert_eq!(
                Command::parse(command).unwrap().check(&cwd),
                Err(Error::Unsupported)
            );
        }
    }

    #[test]
    fn captures_stdout_stderr_and_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        #[cfg(unix)]
        let script = "printf 'hello\\r\\nworld'; printf 'problem' >&2; exit 3";
        #[cfg(windows)]
        let script = "echo hello & echo problem 1>&2 & exit /b 3";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(
                Command::parse(script)
                    .unwrap()
                    .capture(&cwd, CancellationToken::new(), &[]),
            )
            .unwrap();
        assert!(output.stdout.contains("hello"));
        assert!(output.stderr.contains("problem"));
        assert_eq!(output.code, 3);
        #[cfg(unix)]
        assert_eq!(output.stdout, "hello\nworld");
    }

    #[cfg(windows)]
    #[test]
    fn captures_windows_system_utilities() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for file in ["notes.txt", "other.txt"] {
            std::fs::write(cwd.join(file), "ordinary sample line\r\n").unwrap();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let capture = |script| {
            runtime
                .block_on(Command::parse(script).unwrap().capture(
                    &cwd,
                    CancellationToken::new(),
                    &[],
                ))
                .unwrap()
        };

        for script in [r#"find "sample" notes.txt"#, "more notes.txt"] {
            let output = capture(script);
            assert_eq!(output.code, 0, "{script}: {}", output.stderr);
            assert!(output.stdout.contains("ordinary sample line"), "{script}");
        }
        assert_eq!(capture("fc notes.txt other.txt").code, 0);
        let timeout = capture("timeout /t 1");
        assert!(timeout.code == 0 || !timeout.stderr.is_empty());
        let comment = capture("rem an ordinary comment");
        assert_eq!(comment.code, 0);
        assert!(comment.stdout.is_empty());

        let script = "echo \"literal | text\" | findstr literal";
        let output = capture(script);
        assert_eq!(output.code, 0, "{}", output.stderr);
        assert_eq!(output.stdout, "\"literal | text\" \n");

        for unsupported in [
            "echo \"literal | text\" | findstr literal | findstr text",
            "echo \"%PATH%\" | findstr PATH",
        ] {
            assert_eq!(
                Command::parse(unsupported).unwrap().check(&cwd),
                Err(Error::Unsupported),
                "{unsupported}"
            );
        }

        let python = std::process::Command::new("python.exe")
            .args([
                "-c",
                "import subprocess; r=subprocess.run('echo \"literal | text\" | findstr literal', shell=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE); print(r.returncode); print(r.stdout.hex()); print(r.stderr.hex())",
            ])
            .output();
        if let Ok(python) = python {
            assert!(python.status.success());
            let report = String::from_utf8(python.stdout).unwrap();
            let mut lines = report.lines();
            let stock_code: i32 = lines.next().unwrap().parse().unwrap();
            let decode_hex = |hex: &str| {
                (0..hex.len())
                    .step_by(2)
                    .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
                    .collect::<Vec<_>>()
            };
            let stock_stdout = decode_hex(lines.next().unwrap());
            let stock_stderr = decode_hex(lines.next().unwrap());
            assert_eq!(output.code, stock_code);
            assert_eq!(output.stdout.replace("\n", "\r\n").as_bytes(), stock_stdout);
            assert!(stock_stderr.is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn scoped_findstr_matches_native_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for (path, contents) in [
            ("notes.txt", "one two\r\nnext\r\n"),
            ("second.txt", "one\r\n"),
            ("nested/notes.txt", "One two\r\n"),
            ("nested/folder name.txt", "one\r\n"),
        ] {
            let path = cwd.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cmd = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        for script in [
            "findstr /s /i one notes.txt",
            "findstr /s /i missing notes.txt",
            "findstr /s /i one missing.txt",
            "findstr /s /n one *.txt",
            r#"findstr /s /i /c:"one two" .\*.txt"#,
            r#"findstr /n one nested\*.txt"#,
            r#"findstr /n one "nested\folder*.txt""#,
            "findstr /n one *.txt",
            "findstr /n one note?.txt",
            "findstr /n missing *.txt",
            "findstr /n one *.log",
        ] {
            use std::os::windows::process::CommandExt;
            let expected = std::process::Command::new(&cmd)
                .current_dir(&cwd)
                .args(["/d", "/v:off", "/s", "/c"])
                .raw_arg(format!("\"{script}\""))
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let (stdout, stderr, status) = runtime.block_on(async {
                let mut prepared = Command::parse(script).unwrap().prepare(&cwd, true).unwrap();
                let mut child = prepared.command.spawn().unwrap();
                let stdout = child.stdout().take().unwrap();
                let stderr = child.stderr().take().unwrap();
                tokio::try_join!(read(stdout), read(stderr), child.wait()).unwrap()
            });
            assert_eq!(stdout, expected.stdout, "{script}");
            assert_eq!(stderr, expected.stderr, "{script}");
            assert_eq!(status.code(), expected.status.code(), "{script}");
        }

        std::fs::write(cwd.join("café.txt"), "ordinary match\r\n").unwrap();
        assert!(matches!(
            Command::parse("findstr /n ordinary caf*.txt")
                .unwrap()
                .prepare(&cwd, true),
            Err(Error::Unsupported)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn scoped_findstr_preserves_path_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for path in [
            "ordinary.txt",
            "config.yaml",
            "credentials.json",
            "sample.pem",
            ".cache/notes.txt",
        ] {
            let path = cwd.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "ordinary text\r\n").unwrap();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(Command::parse("findstr /s ordinary *").unwrap().capture(
                &cwd,
                CancellationToken::new(),
                &[],
            ))
            .unwrap();
        assert_eq!(output.stdout, "ordinary.txt:ordinary text\n");
    }

    #[test]
    fn captures_standalone_git_reports() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let git = std::process::Command::new(if cfg!(windows) { "git.exe" } else { "git" })
            .arg("--version")
            .output();
        if !git.is_ok_and(|output| output.status.success()) {
            return;
        }
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new(if cfg!(windows) { "git.exe" } else { "git" })
                .current_dir(&cwd)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {:?}", output);
        };
        run_git(&["init", "-q"]);
        std::fs::write(cwd.join("tracked.txt"), "initial\n").unwrap();
        run_git(&["add", "tracked.txt"]);
        run_git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "--no-gpg-sign",
            "-m",
            "initial commit",
        ]);
        std::fs::write(cwd.join("tracked.txt"), "staged\n").unwrap();
        run_git(&["add", "tracked.txt"]);
        std::fs::write(cwd.join("tracked.txt"), "unstaged\n").unwrap();
        run_git(&["config", "format.pretty", "format:local format override"]);
        run_git(&["config", "diff.external", "no-such-composer-test-program"]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (input, expected) in [
            ("git log", "initial commit"),
            ("git log --oneline -n 1", "initial commit"),
            ("git log -1 --oneline", "initial commit"),
            ("git diff --cached --patch -- tracked.txt", "+staged"),
            #[cfg(windows)]
            ("git status --short", "MM tracked.txt"),
            #[cfg(windows)]
            ("git diff --stat", "tracked.txt"),
        ] {
            let output = runtime
                .block_on(Command::parse(input).unwrap().capture(
                    &cwd,
                    CancellationToken::new(),
                    &[],
                ))
                .unwrap_or_else(|error| panic!("{input}: {error:?}"));
            assert_eq!(output.code, 0, "{input}: {}", output.stderr);
            assert!(
                output.stdout.contains(expected),
                "{input}: {}",
                output.stdout
            );
        }
        for input in [
            "git status",
            "git diff",
            "git status --short | git log",
            "git diff --stat --ext-diff",
            "git diff --cached --ext-diff",
            "git log --show-signature",
            "git log --format=%G?",
            "git log | git log",
            "git log > log.txt",
        ] {
            assert_eq!(
                Command::parse(input).unwrap().check(&cwd),
                Err(Error::Unsupported),
                "{input}"
            );
        }
        #[cfg(windows)]
        {
            let subdir = cwd.join("subdir");
            std::fs::create_dir(&subdir).unwrap();
            let outside = tempfile::tempdir().unwrap();
            let outside = outside.path().canonicalize().unwrap();
            run_git(&["config", "core.worktree", outside.to_str().unwrap()]);
            for (input, expected) in [
                ("git status --short", "MM ../tracked.txt"),
                ("git diff --stat", "tracked.txt"),
            ] {
                let output = runtime
                    .block_on(Command::parse(input).unwrap().capture(
                        &subdir,
                        CancellationToken::new(),
                        &[],
                    ))
                    .unwrap();
                assert_eq!(output.code, 0, "{input}: {}", output.stderr);
                assert!(
                    output.stdout.contains(expected),
                    "{input}: {}",
                    output.stdout
                );
                let output = runtime
                    .block_on(Command::parse(input).unwrap().capture(
                        &outside,
                        CancellationToken::new(),
                        &[],
                    ))
                    .unwrap();
                let native = std::process::Command::new("git.exe")
                    .current_dir(&outside)
                    .env("LC_ALL", "C")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "NUL")
                    .args(input.split_ascii_whitespace().skip(1))
                    .output()
                    .unwrap();
                assert!(
                    String::from_utf8_lossy(&native.stderr)
                        .to_ascii_lowercase()
                        .contains("not a git repository")
                );
                assert_eq!(Some(output.code), native.status.code(), "{input}");
                assert_eq!(
                    output.stderr,
                    output_tail(&native.stderr, &[], 2000),
                    "{input}"
                );
            }
            run_git(&["config", "--unset", "core.worktree"]);
            let whoami = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("whoami.exe");
            assert!(whoami.is_file());
            let executable = whoami.to_str().unwrap().replace('\\', "/");
            std::fs::write(cwd.join(".gitattributes"), "tracked.txt filter=fixture\n").unwrap();
            run_git(&["config", "filter.fixture.clean", &executable]);
            run_git(&["config", "filter.fixture.required", "true"]);
            let output = runtime
                .block_on(Command::parse("git diff --stat").unwrap().capture(
                    &cwd,
                    CancellationToken::new(),
                    &[],
                ))
                .unwrap();
            assert_ne!(output.code, 0, "external filter ran: {}", output.stdout);
            assert!(output.stderr.contains("fixture"), "{}", output.stderr);
        }
    }

    #[cfg(unix)]
    #[test]
    fn deadline_stops_command() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(Command::parse("sleep 20").unwrap().capture_for(
            &cwd,
            CancellationToken::new(),
            &[],
            Duration::from_millis(100),
        ));
        assert!(matches!(result, Err(Error::Timeout)));
    }
}
