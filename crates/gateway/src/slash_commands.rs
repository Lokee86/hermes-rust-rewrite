//! Stock-compatible discovery and dispatch for registered Python slash commands.

use super::{Peer, RpcError, RpcResult, Shared, lock, required_string};
use hermes_engine::plugins::{PluginCommand, PluginStatus, PythonPlugin};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

/// Native command names and aliases stay reserved even where behavior is unported.
const STOCK: &[&str] = &[
    "agents",
    "tasks",
    "approvals",
    "archive",
    "archive-tree",
    "battery",
    "bg",
    "background",
    "blueprint",
    "bp",
    "branch",
    "fork",
    "browser",
    "btw",
    "bundles",
    "codex-runtime",
    "codex_runtime",
    "compress",
    "compact",
    "config",
    "context",
    "ctx",
    "debug",
    "diff",
    "egress",
    "export",
    "focus",
    "goal",
    "handoff",
    "hatch",
    "generate-pet",
    "heartbeat",
    "hb",
    "help",
    "commands",
    "import",
    "init",
    "journey",
    "learning",
    "memory-graph",
    "learn",
    "loop",
    "memory",
    "moa",
    "new",
    "reset",
    "palette",
    "pause",
    "personality",
    "pet",
    "plan",
    "platform",
    "profile",
    "prompt",
    "queue",
    "q",
    "reasoning",
    "refine",
    "resume",
    "sessions",
    "switch",
    "retry",
    "review",
    "rollback",
    "save",
    "skills",
    "skin",
    "start",
    "status",
    "steer",
    "s",
    "stop",
    "subgoal",
    "subscription",
    "suggestions",
    "timestamps",
    "ts",
    "title",
    "tools",
    "topic",
    "topup",
    "undo",
    "usage",
    "version",
    "v",
    "wake",
    "whoami",
    "model",
    "busy",
    "copy",
    "cron",
    "footer",
    "gateway",
    "image",
    "identity",
    "indicator",
    "paste",
    "platforms",
    "plugins",
    "redraw",
    "reload",
    "restart",
    "rewind",
    "sb",
    "set-home",
    "sethome",
    "snap",
    "snapshot",
    "statusbar",
    "toolsets",
    "verbose",
    "worktree",
    "yolo",
    "quit",
    "exit",
    "clear",
    "theme",
    "doctor",
    "update",
];

fn canonical(name: &str) -> String {
    name.trim()
        .trim_start_matches('/')
        .to_ascii_lowercase()
        .replace('_', "-")
}

fn plugin_claims<'a>(
    inventory: &'a [PluginStatus],
) -> (
    BTreeMap<String, (&'a PluginStatus, &'a PluginCommand)>,
    HashMap<String, Vec<String>>,
) {
    let mut claims: HashMap<String, Vec<(&PluginStatus, &PluginCommand)>> = HashMap::new();
    for status in inventory {
        if status.issue.is_some() {
            continue;
        }
        for command in &status.commands {
            let name = canonical(&command.name);
            if name.is_empty() || STOCK.contains(&name.as_str()) {
                continue;
            }
            claims.entry(name).or_default().push((status, command));
        }
    }
    let mut conflicts = HashMap::new();
    let mut unique = BTreeMap::new();
    for (name, owners) in claims {
        if owners.len() > 1 {
            let mut labels: Vec<_> = owners.iter().map(|(s, _)| s.title.clone()).collect();
            labels.sort();
            labels.dedup();
            conflicts.insert(name, labels);
        } else if let Some((status, command)) = owners.into_iter().next() {
            unique.insert(name, (status, command));
        }
    }
    (unique, conflicts)
}

impl Shared {
    /// Profile-level discovery works before a chat has been created.
    pub(crate) async fn commands_catalog(&self, _params: &Value, _peer: &Peer) -> RpcResult {
        let inventory = self.plugins.inventory().await;
        let (commands, conflicts) = plugin_claims(&inventory);
        let mut pairs = Vec::new();
        let mut canon = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let mut plugin_commands = BTreeMap::new();
        for (name, (_, command)) in &commands {
            let path = format!("/{name}");
            pairs.push(json!([path, command.description]));
            canon.insert(path.clone(), path.clone());
            metadata.insert(path.clone(), json!({"description":command.description,"argument_mode":command.argument_mode,"args_hint":command.args_hint,"desktop":null}));
            plugin_commands.insert(name.clone(), json!(command.description));
        }
        if self.ego_available() {
            let description = "Review and approve the active Reliquary Ego Identity";
            pairs.push(json!(["/identity", description]));
            canon.insert("/identity".into(), "/identity".into());
            metadata.insert("/identity".into(), json!({"description":description,"argument_mode":"optional","args_hint":"exact text","desktop":null}));
            plugin_commands.insert("identity".into(), json!(description));
        }
        let statuses: Vec<Value> = inventory.iter().map(|status| {
            let conflict_names: Vec<_> = conflicts.iter()
                .filter(|(name, owners)| owners.contains(&status.title) && status.commands.iter().any(|c| canonical(&c.name) == **name))
                .map(|(name, _)| name.clone()).collect();
            json!({"id":status.id,"title":status.title,"issue":status.issue,"unsupported":status.unsupported,"conflicts":conflict_names})
        }).collect();
        Ok(json!({"pairs":pairs,"canon":canon,"commands":metadata,
            "categories":[{"name":"Plugin commands","pairs":pairs}],
            "plugin_commands":plugin_commands,"plugin_status":statuses,"skills":[],"subscriptions":[]}))
    }

    /// Execute a registered handler against an already attached live local session.
    pub(crate) async fn slash_exec(
        self: &std::sync::Arc<Self>,
        params: &Value,
        peer: &Peer,
    ) -> RpcResult {
        if !peer.canonical || !peer.local_native {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Plugin commands require the private local profile client",
            ));
        }
        let raw = required_string(params, "command")?;
        let command = raw.trim().strip_prefix('/').unwrap_or(raw.trim());
        let (word, arguments) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        let name = canonical(word);
        if name.is_empty() {
            return Err(RpcError::invalid("command must name a plugin command"));
        }
        if name != "identity" && STOCK.contains(&name.as_str()) {
            return Err(RpcError::new(
                4004,
                "unknown_command",
                "This command is owned by the native client",
            ));
        }
        let session = self.attached(params, peer).await?;
        let summary = lock(&session.summary).clone();
        if summary.archived {
            return Err(RpcError::new(
                4001,
                "session_archived",
                "Plugin commands require an active session",
            ));
        }
        let settings = lock(&session.settings).clone();
        let source_kind = settings.source.clone();
        if !matches!(
            source_kind.as_str(),
            "cli" | "tui" | "gui" | "desktop" | "rust"
        ) {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Plugin commands are unavailable for this session source",
            ));
        }
        if name == "identity" {
            return self.identity_command(session, peer, arguments).await;
        }
        let inventory = self.plugins.inventory().await;
        let (commands, conflicts) = plugin_claims(&inventory);
        if conflicts.contains_key(&name) {
            return Err(RpcError::new(
                4004,
                "ambiguous_command",
                format!("/{name} is claimed by multiple plugins; disable one owner"),
            ));
        }
        let (status, metadata) = commands.get(&name).ok_or_else(|| {
            RpcError::new(
                4004,
                "unknown_command",
                format!("No enabled plugin provides /{name}"),
            )
        })?;
        let handle: &PythonPlugin = status.handle.as_ref().ok_or_else(|| {
            RpcError::new(
                4004,
                "plugin_unavailable",
                format!("Plugin {} has no active command worker", status.title),
            )
        })?;
        if !self.plugins.is_current(handle).await {
            return Err(RpcError::new(
                4004,
                "plugin_unavailable",
                "Plugin was disabled or replaced; refresh command discovery",
            ));
        }
        let cwd = settings
            .cwd
            .as_ref()
            .and_then(|path| path.to_str())
            .unwrap_or("");
        let session_id = summary.id;
        let session_key = local_session_key(&self.profile_name, &session_id);
        let response = handle.request("hermes/command", json!({"name":metadata.name,"arguments":arguments,
            "context":{"source":{"platform":"local","chat_id":session_id,"chat_type":"dm","profile_name":self.profile_name},
                "session_id":session_id,"session_key":session_key,"platform":"local","cwd":cwd}}))
            .await.map_err(|error| RpcError::new(4004, "command_failed", format!("Plugin command failed: {error}")))?;
        if !self.plugins.is_current(handle).await {
            return Err(RpcError::new(
                4004,
                "plugin_unavailable",
                "Plugin was disabled while the command was running",
            ));
        }
        let output = match response {
            Value::Null => String::new(),
            Value::String(text) => text,
            other => {
                return Err(RpcError::new(
                    4004,
                    "invalid_command_result",
                    format!("Plugin command returned {}, expected text or null", other),
                ));
            }
        };
        Ok(json!({"type":"exec","output":output}))
    }
}

fn local_session_key(profile_name: &str, session_id: &str) -> String {
    let profile = match profile_name {
        "default" => "main",
        "main" => "main~",
        other => other,
    };
    format!("agent:{profile}:local:dm:{session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Gateway, GatewayConfig};
    use axum::{Json, Router, extract::State, routing::post};
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    async fn model_request(
        State(requests): State<Arc<AtomicUsize>>,
        Json(_): Json<Value>,
    ) -> Json<Value> {
        requests.fetch_add(1, Ordering::SeqCst);
        Json(
            json!({"choices":[{"message":{"role":"assistant","content":"unexpected model call"}}]}),
        )
    }

    fn local_peer() -> Peer {
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        Peer {
            id: crate::new_id(),
            tx,
            stop: crate::CancellationToken::new(),
            canonical: true,
            local_native: true,
        }
    }

    async fn rpc(shared: &Arc<Shared>, peer: &Peer, method: &str, params: Value) -> RpcResult {
        shared.dispatch(method, &params, peer).await
    }

    fn status(id: &str, name: &str) -> PluginStatus {
        PluginStatus {
            id: id.into(),
            title: id.into(),
            commands: vec![PluginCommand {
                name: name.into(),
                description: format!("{id} command"),
                args_hint: "[text]".into(),
                argument_mode: None,
            }],
            provider: None,
            issue: None,
            unsupported: Vec::new(),
            handle: None,
        }
    }

    #[test]
    fn names_are_normalized_and_native_names_reserved() {
        assert_eq!(canonical("  /My_Command "), "my-command");
        assert!(STOCK.contains(&canonical("/Codex_Runtime").as_str()));
        assert!(!STOCK.contains(&canonical("/my_plugin").as_str()));
    }

    #[test]
    fn split_preserves_trailing_argument_whitespace() {
        let (name, arguments) = "tool  first arg\twith spacing"
            .split_once(char::is_whitespace)
            .unwrap();
        assert_eq!(name, "tool");
        assert_eq!(arguments, " first arg\twith spacing");
    }

    #[test]
    fn local_session_key_uses_stock_default_profile_mapping() {
        assert_eq!(
            local_session_key("default", "sid-1"),
            "agent:main:local:dm:sid-1"
        );
        assert_eq!(
            local_session_key("main", "sid-1"),
            "agent:main~:local:dm:sid-1"
        );
    }

    #[test]
    fn command_claims_omit_builtin_and_ambiguous_names() {
        let mut partial = status("partial", "still-available");
        partial.unsupported.push("memory provider".into());
        let inventory = vec![
            status("one", "hello_world"),
            status("two", "hello-world"),
            status("three", "help"),
            partial,
        ];
        let (unique, conflicts) = plugin_claims(&inventory);
        assert!(!unique.contains_key("hello-world"));
        assert!(!unique.contains_key("help"));
        assert!(unique.contains_key("still-available"));
        assert_eq!(conflicts.get("hello-world").unwrap(), &["one", "two"]);
    }

    #[tokio::test]
    async fn attached_clients_discover_and_execute_local_plugin_without_model_turn() {
        if hermes_engine::plugins::python_host().is_none() {
            return;
        }
        let home = TempDir::new().unwrap();
        let plugin_dir = home.path().join("plugins/fixture-command");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("plugin.yaml"),
            "name: fixture-command\nversion: 0.0.1\n",
        )
        .unwrap();
        fs::write(
            plugin_dir.join("__init__.py"),
            r#"
import json

def execute(args, *, source=None, session_id=None, session_key=None, platform=None):
    return json.dumps({"args": args, "source": vars(source), "session_id": session_id,
        "session_key": session_key, "platform": platform})

def register(host):
    host.register_command("fixture-greet", execute, description="Fixture greeting",
        args_hint="<text>", argument_mode="mixed")
"#,
        )
        .unwrap();
        fs::write(
            home.path().join("plugins/installed.json"),
            json!([{
                "name":"fixture-command", "title":"Fixture Command", "version":"0.0.1",
                "description":"", "sha":"fixture", "enabled":true,
                "servers":["python"], "skills":[]
            }])
            .to_string(),
        )
        .unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let model_address = listener.local_addr().unwrap();
        let model_requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/chat/completions", post(model_request))
            .with_state(model_requests.clone());
        let model = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = Config {
            base_url: format!("http://{model_address}/v1"),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
        };
        let gateway = Gateway::start(GatewayConfig::new(home.path(), config))
            .await
            .unwrap();
        let first = local_peer();
        let second = local_peer();

        let first_catalog = rpc(&gateway.shared, &first, "commands.catalog", json!({}))
            .await
            .unwrap();
        let second_catalog = rpc(&gateway.shared, &second, "commands.catalog", json!({}))
            .await
            .unwrap();
        assert_eq!(first_catalog, second_catalog);
        assert_eq!(
            first_catalog["plugin_commands"]["fixture-greet"],
            "Fixture greeting"
        );
        assert_eq!(
            first_catalog["commands"]["/fixture-greet"]["argument_mode"],
            "mixed"
        );

        let created = rpc(
            &gateway.shared,
            &first,
            "session.create",
            json!({"source":"gui"}),
        )
        .await
        .unwrap();
        let session_id = created["session_id"]
            .as_str()
            .or_else(|| created["stored_session_id"].as_str())
            .expect("session.create returns its session id")
            .to_owned();
        rpc(
            &gateway.shared,
            &second,
            "session.resume",
            json!({"session_id":session_id}),
        )
        .await
        .unwrap();
        let executed = rpc(
            &gateway.shared,
            &second,
            "slash.exec",
            json!({
                "session_id":session_id, "command":"FIXTURE_GREET  hello world"
            }),
        )
        .await
        .unwrap();
        assert_eq!(executed["type"], "exec");
        let callback: Value = serde_json::from_str(executed["output"].as_str().unwrap()).unwrap();
        assert_eq!(callback["args"], " hello world");
        assert_eq!(callback["session_id"], session_id);
        assert_eq!(
            callback["session_key"],
            local_session_key(&gateway.shared.profile_name, &session_id)
        );
        assert_eq!(callback["platform"], "local");
        assert_eq!(callback["source"]["platform"], "local");
        assert_eq!(callback["source"]["chat_id"], session_id);
        assert_eq!(callback["source"]["chat_type"], "dm");
        assert_eq!(
            callback["source"]["profile_name"],
            gateway.shared.profile_name
        );

        assert!(
            rpc(
                &gateway.shared,
                &first,
                "slash.exec",
                json!({
                    "session_id":session_id, "command":"prefix-fixture-greet hi"
                })
            )
            .await
            .is_err()
        );
        hermes_engine::plugins::set_enabled(
            gateway.shared.library.root(),
            "fixture-command",
            false,
        )
        .unwrap();
        let refreshed = rpc(&gateway.shared, &first, "commands.catalog", json!({}))
            .await
            .unwrap();
        assert!(refreshed["plugin_commands"]["fixture-greet"].is_null());
        assert!(
            rpc(
                &gateway.shared,
                &second,
                "slash.exec",
                json!({
                    "session_id":session_id, "command":"fixture-greet hi"
                })
            )
            .await
            .is_err()
        );
        assert_eq!(model_requests.load(Ordering::SeqCst), 0);
        gateway.shutdown().await.unwrap();
        model.abort();
    }
}
