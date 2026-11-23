//! Human-initiated captured shell commands for a proven local session.
use super::*;
use hermes_engine::shell::{Command, Error};

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Policy {
    pub cwd: PathBuf,
    pub backend: String,
}

#[derive(Default, Deserialize)]
struct Profile {
    terminal: Option<Terminal>,
}

#[derive(Deserialize)]
struct Terminal {
    backend: Option<String>,
}

pub(super) fn backend(profile: &Path) -> String {
    let file = match File::open(profile.join("config.yaml")) {
        Ok(file) => match serde_yaml::from_reader::<_, Option<Profile>>(file) {
            Ok(config) => config.unwrap_or_default().terminal.and_then(|t| t.backend),
            Err(_) => return "unrecognized".into(),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return "unrecognized".into(),
    };
    let environment = std::env::var("TERMINAL_ENV").ok();
    // Preserve either declaration of a nonlocal backend. Unknown config never
    // falls through to a host subprocess.
    file.into_iter()
        .chain(environment)
        .map(|name| name.trim().to_ascii_lowercase())
        .find(|name| !name.is_empty() && name != "local")
        .map(|name| {
            if name.len() <= 32
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                name
            } else {
                "unrecognized".into()
            }
        })
        .unwrap_or_else(|| "local".into())
}

fn refusal(reason: &str) -> RpcError {
    RpcError::new(4001, reason, reason)
}

pub(super) async fn exec(shared: &Shared, params: &Value, peer: &Peer) -> RpcResult {
    let object = params
        .as_object()
        .ok_or_else(|| refusal("invalid_params"))?;
    let profile = params["profile"].as_str().unwrap_or_default();
    if !profile.is_empty() && profile != shared.profile_name {
        return Err(refusal("profile_mismatch"));
    }
    let mut fields = Vec::new();
    for name in ["session_id", "command", "profile"] {
        if let Some(value) = object.get(name) {
            if !(value.is_string() || (name == "profile" && value.is_null())) {
                fields.push(name);
            }
        }
    }
    fields.extend(
        object
            .keys()
            .map(String::as_str)
            .filter(|key| !matches!(*key, "session_id" | "command" | "profile")),
    );
    if !fields.is_empty() {
        let mut error = refusal("invalid_params");
        error.data["fields"] = json!(fields);
        return Err(error);
    }
    let sid = params["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| refusal("invalid_params"))?;
    let command = params["command"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| refusal("invalid_params"))?;
    if !peer.local_native {
        return Err(refusal("permission_denied"));
    }
    if shared.shutdown.is_cancelled() {
        return Err(refusal("runtime_draining"));
    }
    // A private client may capture by ID without subscribing to a turn or opening an engine.
    let directory = shared
        .library
        .session_dir(sid)
        .map_err(|_| refusal("not_found"))?;
    let policy = Settings::read(&directory)
        .map_err(RpcError::internal)?
        .shell_policy
        .ok_or_else(|| refusal("permission_denied"))?;
    if policy.backend != "local" {
        let mut error = refusal("unsupported_terminal_backend");
        error.data["field"] = json!("terminal.backend");
        error.data["detail"] = json!(policy.backend);
        return Err(error);
    }
    let provider_key = shared
        .sessions
        .lock()
        .await
        .get(sid)
        .map(|session| lock(&session.config).api_key.clone())
        .unwrap_or_else(|| shared.provider.api_key.clone());
    let core = Command::parse(command).map_err(map_error)?;
    let protected = [
        provider_key.as_str(),
        shared.token.as_str(),
        shared.auth.deployment_secret().unwrap_or_default(),
    ];
    let result = core
        .capture(&policy.cwd, peer.stop.clone(), &protected)
        .await
        .map_err(map_error)?;
    serde_json::to_value(result).map_err(RpcError::internal)
}

fn map_error(error: Error) -> RpcError {
    refusal(match error {
        Error::Invalid => "invalid_params",
        Error::Unsupported => "dangerous_command",
        Error::CwdUnavailable => "cwd_unavailable",
        Error::Timeout => "command_timeout",
        Error::Failed => "command_failed",
    })
}
