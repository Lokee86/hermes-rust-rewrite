//! Local owner discovery uses the stock one-request-per-connection protocol.
mod cli;
#[cfg(unix)]
#[path = "unix.rs"]
mod transport;
#[cfg(windows)]
#[path = "windows.rs"]
mod transport;
use crate::{Shared, client::GatewayClient};
use anyhow::{Context, Result, bail, ensure};
pub use cli::{dispatch_role, serve};
use hermes_engine::Config;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions}, io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const SUPERVISOR: &str = "none";

pub(super) fn try_profile_lock(root: &Path) -> Result<Option<File>> {
    let _renames = lock_profile_renames(root)?;
    try_local_owner_lock(root)
}

/// Serialize startup with profile moves. The file is outside the directory
/// being renamed, since Windows rejects a move with the owner file open.
pub(super) fn lock_profile_renames(root: &Path) -> Result<Option<File>> {
    let Some(profiles) = root.parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "profiles")) else {
        return Ok(None);
    };
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(profiles.join(".gateway-profile-renames.lock"))?;
    file.lock()?;
    Ok(Some(file))
}

pub(super) fn try_local_owner_lock(root: &Path) -> Result<Option<File>> {
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(root.join("gateway-owner.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(err)) => Err(err.into()),
    }
}

/// Advance the profile's owner epoch while its exclusive owner lock is held.
pub(super) fn advance_authority_epoch(root: &Path) -> Result<u64> {
    let path = root.join("gateway-authority-epoch");
    let previous = match File::open(&path) {
        Ok(file) => {
            let mut raw = String::new();
            file.take(64).read_to_string(&mut raw)?;
            ensure!(raw.len() < 64, "Invalid gateway authority epoch state");
            raw.trim().parse::<u64>().context("Invalid gateway authority epoch state")?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error).context("Could not read gateway authority epoch"),
    };
    let epoch = previous.checked_add(1).filter(|epoch| *epoch <= (1_u64 << 53) - 1)
        .context("Gateway authority epoch exhausted")?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    writeln!(file, "{epoch}")?;
    file.as_file().sync_all()?;
    file.persist(&path).context("Could not publish gateway authority epoch")?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(epoch)
}

#[derive(Debug, Clone)]
pub struct OwnerConnection {
    pub origin: String,
    profile: String,
    control_home: PathBuf,
    instance: String,
    authority_epoch: u64,
    native: bool,
}

impl OwnerConnection {
    pub fn is_native(&self) -> bool {
        self.native
    }
    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    async fn request(&self, verb: &str, params: Value) -> Result<Value> {
        query(&self.control_home, verb, params)
            .await?
            .context("The gateway has exited")
    }
    fn identity(&self) -> Value {
        json!({"profile_id":self.profile, "instance_id":self.instance})
    }
    pub async fn connect(&self, client: &GatewayClient) -> Result<()> {
        let mut params = self.identity();
        params["purpose"] = json!("interactive");
        let grant = self.request("session-ticket", params).await?;
        ensure!(
            grant["instance_id"] == self.instance
                && grant["profile_id"] == self.profile
                && grant["runtime_protocol"] == 1
                && grant.get("authority_epoch").is_none_or(|epoch| epoch.as_u64() == Some(self.authority_epoch)),
            "Gateway returned a ticket for a different owner"
        );
        let ticket = grant["ticket"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 256)
            .context("Missing gateway ticket")?;
        client.connect_local_grant(&self.origin, ticket).await?;
        Ok(())
    }
    pub async fn local_token(&self) -> Result<String> {
        ensure!(
            self.native,
            "This gateway issues interactive session tickets only"
        );
        let reply = self.request("rust.local-token", self.identity()).await?;
        Ok(reply["token"]
            .as_str()
            .context("Missing local token")?
            .to_owned())
    }
    pub async fn terminal_ticket(&self) -> Result<(String, String)> {
        let token = self.local_token().await?;
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(CONTROL_TIMEOUT)
            .build()?
            .post(format!("{}/api/webapp/socket-ticket", self.origin))
            .query(&[("socket", "host-terminal")])
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
        let body: Value = response.json().await?;
        Ok((
            self.origin.clone(),
            body["ticket"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Missing host-terminal ticket")?
                .to_owned(),
        ))
    }
    pub async fn stop(&self) -> Result<()> {
        ensure!(
            self.native,
            "Manage the stock gateway with its own service controls"
        );
        self.request("rust.stop", self.identity()).await?;
        self.wait_stopped().await
    }
    async fn wait_stopped(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            // A draining owner can close a control connection without answering it (on Unix
            // the listener's backlog is dropped), which means it is not gone yet.
            match discover(Path::new(&self.profile)).await {
                Ok(owner) if owner.as_ref().is_none_or(|o| o.instance != self.instance) => return Ok(()),
                Err(e) if Instant::now() >= deadline => return Err(e.context("Gateway is still shutting down")),
                _ => {}
            }
            ensure!(Instant::now() < deadline, "Gateway is still shutting down");
            sleep(Duration::from_millis(80)).await;
        }
    }
}

pub fn profile_id(root: &Path) -> Result<String> {
    let root = fs::canonicalize(root)?;
    let id = root.to_str().context("Profile path is not valid UTF-8")?;
    #[cfg(windows)]
    {
        if let Some(unc) = id.strip_prefix(r"\\?\UNC\") {
            return Ok(format!(r"\\{unc}"));
        }
        return Ok(id.strip_prefix(r"\\?\").unwrap_or(id).to_owned());
    }
    #[cfg(not(windows))]
    Ok(id.to_owned())
}

async fn query(home: &Path, verb: &str, params: Value) -> Result<Option<Value>> {
    timeout(CONTROL_TIMEOUT, async {
        let mut stream = match transport::connect(home).await {
            Ok(stream) => stream,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e).context("Could not connect to the profile control endpoint"),
        };
        send_line(
            &mut stream,
            &json!({"protocol":1,"id":1,"verb":verb,"params":params}),
        )
        .await?;
        let reply = read_line(stream, 512 * 1024).await?;
        ensure!(
            reply["protocol"] == 1 && reply["id"] == 1,
            "Invalid local control response"
        );
        ensure!(
            reply["ok"] == true,
            "Gateway control refused {verb}: {}",
            reply["error"].as_str().unwrap_or("invalid response")
        );
        Ok(Some(reply["result"].clone()))
    })
    .await
    .context("Local gateway control deadline exceeded")?
}

async fn send_line(stream: &mut (impl AsyncWrite + Unpin), v: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(v)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}
async fn read_line(stream: impl AsyncRead + Unpin, limit: usize) -> Result<Value> {
    let mut buf = Vec::new();
    BufReader::new(stream.take(limit as u64 + 1))
        .read_until(b'\n', &mut buf)
        .await?;
    ensure!(
        buf.last() == Some(&b'\n') && buf.len() <= limit,
        "Invalid control frame length"
    );
    Ok(serde_json::from_slice(&buf)?)
}

fn same_profile(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        left.to_lowercase() == right.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

async fn resolve(home: &Path, selected: &str, parent: bool) -> Result<Option<OwnerConnection>> {
    let Some(reply) = query(home, "resolve-profile", json!({"home":selected})).await? else {
        return Ok(None);
    };
    // A stand-alone owner at the parent home can coexist with a named profile.
    if parent
        && reply["update_fenced"] != true
        && (reply["state"] == "not_served"
            || (reply["state"] == "missing" && reply["control"] == "host"))
    {
        return Ok(None);
    }
    ensure!(
        reply["resolve_protocol"] == 1
            && reply["attachable"] == true
            && reply["update_fenced"] != true,
        "Local owner cannot attach this profile: {}",
        reply["message"].as_str().unwrap_or("not ready")
    );
    let endpoint = &reply["endpoint"];
    let profile = endpoint["profile_id"]
        .as_str()
        .context("Missing owner profile")?;
    ensure!(
        same_profile(profile, selected),
        "Control endpoint returned a different profile"
    );
    ensure!(
        endpoint["runtime_protocol"] == 1
            && endpoint["ws_protocol"] == crate::PROTOCOL
            && endpoint["ticket_protocol_prefix"] == crate::TICKET_PREFIX,
        "Incompatible local gateway"
    );
    let origin = endpoint["api_origin"]
        .as_str()
        .context("Missing local gateway origin")?;
    let origin = crate::client::local_origin(origin)?
        .origin()
        .ascii_serialization();
    let instance = endpoint["instance_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Missing owner instance")?
        .to_owned();
    let authority_epoch = endpoint["authority_epoch"].as_u64().filter(|epoch| *epoch > 0)
        .context("Missing owner authority epoch")?;
    Ok(Some(OwnerConnection {
        origin,
        profile: profile.into(),
        control_home: home.into(),
        instance,
        authority_epoch,
        native: endpoint["implementation"] == "hermes-rust",
    }))
}

pub async fn discover(root: &Path) -> Result<Option<OwnerConnection>> {
    let id = profile_id(root)?;
    if let Some(owner) = resolve(root, &id, false).await? {
        return Ok(Some(owner));
    }
    if let Some(parent) = root
        .parent()
        .filter(|p| p.file_name().is_some_and(|s| s == "profiles"))
        .and_then(Path::parent)
    {
        return resolve(parent, &id, true).await;
    }
    Ok(None)
}

pub async fn ensure_local(root: &Path, provider: &Config) -> Result<OwnerConnection> {
    ensure_with(root, provider, &std::env::current_exe()?).await
}

/// Apply an explicitly selected provider to this profile's managed owner.
/// Model selection is per session and never restarts the owner. Credentials
/// are represented only by a digest on the private control endpoint.
pub async fn ensure_configured(root: &Path, provider: &Config) -> Result<OwnerConnection> {
    provider.validate()?;
    for _ in 0..3 {
        let owner = ensure_local(root, provider).await?;
        ensure!(owner.is_native(), "Change provider settings through the existing gateway's own controls");
        let mut params = owner.identity();
        params["configuration"] = json!(provider_identity(provider, owner.instance_id()));
        let reply = owner.request("rust.configure-provider", params).await?;
        if reply["configured"] == true { return Ok(owner); }
        ensure!(reply["stopping"] == true, "The gateway did not acknowledge provider configuration");
        owner.wait_stopped().await?;
    }
    bail!("Another client changed the provider; retry your selection")
}

fn provider_identity(provider: &Config, instance: &str) -> String {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    digest.update(instance.as_bytes());
    digest.update(&[0]);
    digest.update(provider.base_url.trim().trim_end_matches('/').as_bytes());
    digest.update(&[0]);
    digest.update(provider.api_key.trim().as_bytes());
    digest.finish().as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}
async fn ensure_with(root: &Path, provider: &Config, exe: &Path) -> Result<OwnerConnection> {
    fs::create_dir_all(root)?;
    if let Some(owner) = discover(root).await? {
        return Ok(owner);
    }
    let mut command = Command::new(exe);
    command
        .args(["gateway","run"])
        .args(["--port", "0", "--ephemeral-token", "--data-dir"])
        .arg(root)
        .env("HERMES_RUST_DATA_DIR", root)
        .env("HERMES_RUST_BASE_URL", &provider.base_url)
        .env("HERMES_RUST_MODEL", &provider.model)
        .env("HERMES_RUST_API_KEY", &provider.api_key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .context("Could not launch the profile gateway")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(owner) = discover(root).await? {
            // Reap the owner when it later exits, including on Unix.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(owner);
        }
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "Gateway startup failed ({status})");
        }
        ensure!(
            Instant::now() < deadline,
            "The profile gateway did not publish its control endpoint"
        );
        sleep(Duration::from_millis(80)).await;
    }
}

async fn serve_control(
    mut listener: transport::Listener,
    home: &Path,
    shared: Arc<Shared>,
    stop: CancellationToken,
) -> Result<()> {
    let profile = Arc::new(profile_id(home)?);
    loop {
        let stream = listener.accept().await?;
        let profile = profile.clone();
        let shared = shared.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let _ = timeout(CONTROL_TIMEOUT, async {
                let mut stream = stream;
                let req = read_line(&mut stream, 64 * 1024).await?;
                let (reply, stopping) = control_reply(&req, &profile, &shared).await;
                // A lost reply must still complete an acknowledged drain.
                let _stop_when_done = stopping.then(|| stop.clone().drop_guard());
                send_line(&mut stream, &reply).await?;
                Result::<()>::Ok(())
            })
            .await;
        });
    }
}

async fn control_reply(req: &Value, home: &str, shared: &Shared) -> (Value, bool) {
    let id = &req["id"];
    let instance = &shared.instance_id;
    let params = &req["params"];
    let mut stop = false;
    let result = async {
        ensure!(
            req["protocol"] == 1
                && req.as_object().is_some_and(|o| o
                    .keys()
                    .all(|k| matches!(k.as_str(), "id" | "protocol" | "verb" | "params"))),
            "invalid control envelope"
        );
        let origin = format!("http://{}", shared.address);
        match req["verb"].as_str() {
            Some("identify") => Ok(
                json!({"runtime_protocol":1,"state":"ready","instance_id":instance,"authority_epoch":shared.authority_epoch,"api_origin":origin,
                "pid":std::process::id(),"hermes_home":home,"multiplex":false,"served_profiles":[{"home":home,"profile_id":home}],
                "supervisor":SUPERVISOR,"capabilities":crate::RUNTIME_CAPABILITIES}),
            ),
            Some("resolve-profile") => {
                ensure!(params.is_object(), "invalid_profile");
                let wanted = params["home"].as_str().context("missing selected home")?;
                let same = same_profile(wanted, home);
                if !same {
                    return Ok(
                        json!({"resolve_protocol":1,"attachable":false,"state":"not_served","message":"standalone owner serves another profile","endpoint":null}),
                    );
                }
                Ok(
                    json!({"resolve_protocol":1,"state":"ready","attachable":true,"update_fenced":false,"control":"self","message":"gateway ready",
                    "endpoint":{"profile_id":home,"instance_id":instance,"authority_epoch":shared.authority_epoch,"runtime_protocol":1,"api_origin":origin,
                    "ws_url":format!("ws://{}/api/ws",shared.address),"ws_protocol":crate::PROTOCOL,"ticket_protocol_prefix":crate::TICKET_PREFIX,
                    "capabilities":crate::RUNTIME_CAPABILITIES,"implementation":"hermes-rust","supervisor":SUPERVISOR,"control_home":null}}),
                )
            }
            Some(verb @ ("session-ticket" | "rust.local-token" | "rust.stop")) => {
                ensure!(
                    params["profile_id"] == home && params["instance_id"] == instance.as_str(),
                    "owner_identity_mismatch"
                );
                match verb {
                    "session-ticket" => {
                        ensure!(params["purpose"] == "interactive", "unsupported_purpose");
                        ensure!(
                            params.as_object().is_some_and(|obj| obj.keys().all(|key|
                                matches!(key.as_str(), "profile_id" | "instance_id" | "purpose" | "scope" | "restart_wait")))
                                && params.get("scope").is_none_or(|scope| scope == "profile")
                                && params.get("restart_wait").is_none_or(|policy| policy == "refuse"),
                            "PermissionError: invalid bootstrap parameters"
                        );
                        ensure!(!shared.shutdown.is_cancelled(), "owner_draining");
                        let ticket = shared.issue_ticket().context("too_many_tickets")?;
                        Ok(
                            json!({"ticket":ticket,"profile_id":home,"instance_id":instance,"authority_epoch":shared.authority_epoch,
                                "runtime_protocol":1,"expires_in_seconds":crate::TICKET_TTL.as_secs()}),
                        )
                    }
                    "rust.local-token" => Ok(json!({"token":shared.token})),
                    _ => {
                        stop = true;
                        Ok(json!({"stopping":true}))
                    }
                }
            }
            Some("rust.configure-provider") => {
                ensure!(params["profile_id"] == home && params["instance_id"] == instance.as_str(), "owner_identity_mismatch");
                let configuration = params["configuration"].as_str().context("Missing provider configuration")?;
                ensure!(configuration.len() == 64 && configuration.bytes().all(|byte| byte.is_ascii_hexdigit()), "Invalid provider configuration");
                ensure!(!shared.shutdown.is_cancelled(), "owner_draining");
                if configuration == provider_identity(&shared.provider, instance) {
                    return Ok(json!({"configured":true}));
                }
                let sessions = shared.sessions.lock().await;
                let _admissions = super::lock(&shared.admissions);
                ensure!(!shared.shutdown.is_cancelled(), "owner_draining");
                ensure!(shared.scheduler.gateway_claims()?.is_empty(), "Wait for scheduled gateway work to finish before changing providers");
                ensure!(sessions.values().all(|session| {
                    let view = super::lock(&session.view);
                    !view.running && view.authority.receipts.iter().all(|receipt| receipt.status == "terminal")
                }), "Stop or settle pending gateway work before changing providers");
                for summary in shared.library.authority_sessions()? {
                    if sessions.contains_key(&summary.id) { continue; }
                    match File::open(summary.directory.join("conversation.json")) {
                        Ok(file) => {
                            let saved: Value = serde_json::from_reader(file)?;
                            ensure!(saved["authority"]["receipts"].as_array().into_iter().flatten()
                                .all(|receipt| receipt["status"] == "terminal"),
                                "Stop or settle pending gateway work before changing providers");
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    if let Ok(file) = File::open(summary.directory.join("queued-prompts.json")) {
                        let saved: Value = serde_json::from_reader(file)?;
                        ensure!(saved["entries"].as_array().is_none_or(Vec::is_empty) && saved["in_flight"].is_null(),
                            "Stop or settle pending gateway work before changing providers");
                    }
                }
                // Fence new admissions before acknowledging the restart. The
                // selected endpoint can never be applied to an old queued turn.
                shared.shutdown.cancel();
                stop = true;
                Ok(json!({"stopping":true}))
            }
            _ => bail!("unknown_verb"),
        }
    }.await;
    match result {
        Ok(result) => (
            json!({"ok":true,"id":id,"protocol":1,"result":result}),
            stop,
        ),
        Err(error) => (
            json!({"ok":false,"id":id,"protocol":1,"error":error.to_string()}),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gateway, GatewayConfig, client::ClientOptions};

    #[test]
    fn provider_comparison_is_instance_scoped_and_ignores_model_selection() {
        let mut config = Config { base_url: "https://api.openai.com/v1/".into(), model: "first".into(), api_key: "synthetic-key".into() };
        let first = provider_identity(&config, "owner-a");
        config.model = "second".into();
        config.base_url.pop();
        assert_eq!(first, provider_identity(&config, "owner-a"));
        assert_ne!(first, provider_identity(&config, "owner-b"));
        config.api_key.push('2');
        assert_ne!(first, provider_identity(&config, "owner-a"));
        assert!(!first.contains("synthetic-key"));
    }

    #[tokio::test]
    async fn provider_restart_refuses_work_then_fences_late_admission() {
        let directory = tempfile::tempdir().unwrap();
        let home = profile_id(directory.path()).unwrap();
        let config = Config { base_url: "http://127.0.0.1:1/v1".into(), model: "first".into(), api_key: "synthetic-key".into() };
        let gateway = Gateway::start(GatewayConfig::new(directory.path(), config.clone())).await.unwrap();
        let summary = gateway.shared.library.create_session(None, None).unwrap();
        let session = gateway.shared.session(&summary.id).await.unwrap_or_else(|error| panic!("{}", error.message));
        super::super::lock(&session.view).running = true;
        let request = |provider: &Config| json!({"id":1,"protocol":1,"verb":"rust.configure-provider","params":{
            "profile_id":home,"instance_id":gateway.shared.instance_id,
            "configuration":provider_identity(provider, &gateway.shared.instance_id)}});
        let mut model_only = config.clone(); model_only.model = "second".into();
        let (same, stopping) = control_reply(&request(&model_only), &home, &gateway.shared).await;
        assert_eq!(same["result"]["configured"], true); assert!(!stopping);
        let mut changed = config.clone(); changed.base_url = "http://127.0.0.1:2/v1".into();
        let (busy, stopping) = control_reply(&request(&changed), &home, &gateway.shared).await;
        assert_eq!(busy["ok"], false); assert!(!stopping); assert!(!gateway.shared.shutdown.is_cancelled());
        super::super::lock(&session.view).running = false;
        let (ready, stopping) = control_reply(&request(&changed), &home, &gateway.shared).await;
        assert_eq!(ready["result"]["stopping"], true); assert!(stopping);
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let peer = crate::Peer { id: "late-client".into(), tx, stop: CancellationToken::new(), canonical: true, local_native: true };
        super::super::lock(&session.peers).insert(peer.id.clone(), peer.clone());
        let late = gateway.shared.submit(&json!({"session_id":summary.id,"text":"late","submission_id":"late"}), &peer).await;
        assert!(late.is_err());
        assert!(super::super::lock(&session.view).authority.receipts.is_empty());
        gateway.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stock_discovery_and_ticket_name_the_same_ready_authority() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().to_owned();
        let profile = profile_id(&home).unwrap();
        let lock = try_profile_lock(&home).unwrap().unwrap();
        let gateway = Gateway::start_profile(
            GatewayConfig::new(&home, Config::from_env()), profile.clone(), "default".into(), lock,
        ).await.unwrap();
        assert!(try_profile_lock(&home).unwrap().is_none());
        let listener = transport::Listener::bind(&home).unwrap();
        let shared = gateway.shared.clone();
        let stop = CancellationToken::new();
        let serve = tokio::spawn({
            let home = home.clone();
            let stop = stop.clone();
            async move {
                tokio::select! {
                    _ = stop.cancelled() => {},
                    result = serve_control(listener, &home, shared, stop.clone()) => result.unwrap(),
                }
            }
        });

        let identifying = query(&home, "identify", json!({})).await.unwrap().unwrap();
        let resolved = query(&home, "resolve-profile", json!({"home":profile})).await.unwrap().unwrap();
        let endpoint = &resolved["endpoint"];
        assert_eq!(identifying["supervisor"], "none");
        assert_eq!(identifying["capabilities"], json!(crate::RUNTIME_CAPABILITIES));
        for key in ["runtime_protocol", "authority_epoch", "instance_id", "api_origin", "supervisor", "capabilities"] {
            assert_eq!(identifying[key], endpoint[key], "{key}");
        }
        assert_eq!(endpoint["profile_id"], profile);
        assert!(endpoint["authority_epoch"].as_u64().is_some_and(|epoch| epoch > 0));

        let owner = discover(&home).await.unwrap().unwrap();
        let mut request = owner.identity();
        request["purpose"] = json!("interactive");
        request["scope"] = json!("profile");
        request["restart_wait"] = json!("refuse");
        let grant = query(&home, "session-ticket", request.clone()).await.unwrap().unwrap();
        for key in ["runtime_protocol", "authority_epoch", "instance_id", "profile_id"] {
            assert_eq!(grant[key], endpoint[key], "{key}");
        }
        request["scope"] = json!("receipt-only");
        assert!(query(&home, "session-ticket", request.clone()).await.is_err());
        request["scope"] = json!("profile");
        request["subject"] = json!("unimplemented-forwarding");
        assert!(query(&home, "session-ticket", request.clone()).await.is_err());
        request.as_object_mut().unwrap().remove("subject");
        request["instance_id"] = json!("stale-owner");
        assert!(query(&home, "session-ticket", request).await.is_err());

        let client = GatewayClient::new(ClientOptions::default());
        owner.connect(&client).await.unwrap();
        let described = client.request("runtime.describe", json!({})).await.unwrap();
        for key in ["authority_epoch", "instance_id", "profile_id", "capabilities"] {
            assert_eq!(described[key], endpoint[key], "{key}");
        }
        let created = client.request("session.create", json!({"source":"tui"})).await.unwrap();
        assert_eq!(created["authority_epoch"], endpoint["authority_epoch"]);
        client.close();
        stop.cancel();
        serve.await.unwrap();
        gateway.shutdown().await.unwrap();
    }
}
