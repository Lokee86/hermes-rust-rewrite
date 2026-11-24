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
    "model",
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
        if status.issue.is_some() || !status.unsupported.is_empty() {
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
            metadata.insert(path.clone(), json!({"argument_mode":command.argument_mode,"args_hint":command.args_hint,"desktop":null}));
            plugin_commands.insert(name.clone(), json!({"description":command.description,"args_hint":command.args_hint,"argument_mode":command.argument_mode}));
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
    pub(crate) async fn slash_exec(&self, params: &Value, peer: &Peer) -> RpcResult {
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
        if STOCK.contains(&name.as_str()) {
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
        let source = settings.source.clone();
        if !matches!(source.as_str(), "cli" | "tui" | "gui" | "desktop" | "rust") {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Plugin commands are unavailable for this session source",
            ));
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
        let response = handle.request("hermes/command", json!({"name":metadata.name,"arguments":arguments,
            "context":{"source":source,"session_id":session_id,"session_key":session_id,"platform":"local","cwd":cwd}}))
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn command_claims_omit_builtin_and_ambiguous_names() {
        let inventory = vec![
            status("one", "hello_world"),
            status("two", "hello-world"),
            status("three", "help"),
        ];
        let (unique, conflicts) = plugin_claims(&inventory);
        assert!(!unique.contains_key("hello-world"));
        assert!(!unique.contains_key("help"));
        assert_eq!(conflicts.get("hello-world").unwrap(), &["one", "two"]);
    }
}
