//! Windows half of the combined two-window acceptance in interaction_tests.
use super::Action;
use crate::{Message, Screen, interaction_tests::Window};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) struct Probe {
    workspace: PathBuf,
    id: String,
    initial: Value,
}

async fn proof(path: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(bytes) = std::fs::read(path)
                && let Ok(proof) = serde_json::from_slice(&bytes)
            {
                return proof;
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Timed out waiting for {}", path.display()))
}

fn type_line(window: &mut Window, line: &str) {
    window.dispatch(Message::Terminal(Action::Input(
        format!("{line}\r").into_bytes(),
    )));
}

async fn attach(window: &mut Window) {
    window.dispatch(Message::Navigate(Screen::Terminal));
    window
        .until(|app| {
            app.terminal.error.is_some()
                || app
                    .terminal
                    .current
                    .and_then(|id| app.terminal.tabs.get(&id))
                    .is_some_and(|t| {
                        (t.terminal_id.is_some() && t.commands.is_some() && t.notice.is_none())
                            || (t.generation > 0 && t.commands.is_none())
                    })
        })
        .await;
    let pane = &window.app.terminal;
    assert!(pane.error.is_none(), "terminal UI error: {:?}", pane.error);
    let tab = &pane.tabs[&pane.current.unwrap()];
    assert!(
        tab.commands.is_some() && tab.notice.is_none(),
        "terminal attach: {:?}",
        tab.notice
    );
}

pub(crate) async fn open(window: &mut Window) -> Probe {
    let workspace = window
        .app
        .workspace()
        .expect("fixture needs a real project workspace");
    std::fs::write(workspace.join("terminal-probe.py"), r#"import json, os, sys
from pathlib import Path
def write(name, value):
    Path(name).write_text(json.dumps(value))
state = dict(pid=os.getpid(), parent=os.getppid(), cwd=os.getcwd(), variable=os.getenv("HERMES_VIEWER_PROBE"))
if sys.argv[1] == "shell":
    write("shell.json", state)
else:
    write("start.json", state)
    state["line"] = input()
    write("reply.json", state)
"#).unwrap();
    attach(window).await;
    let pane = &window.app.terminal;
    let tab = &pane.tabs[&pane.current.unwrap()];
    let id = tab.terminal_id.clone().unwrap();
    if tab.name.to_ascii_lowercase().contains("cmd") {
        type_line(window, "set HERMES_VIEWER_PROBE=retained");
    } else {
        type_line(window, "$env:HERMES_VIEWER_PROBE='retained'");
    }
    type_line(window, "python -u terminal-probe.py child");
    let initial = proof(&workspace.join("start.json")).await;
    assert!(initial["pid"].as_u64().unwrap() > 0 && initial["parent"].as_u64().unwrap() > 0);
    assert_eq!(initial["variable"], "retained");
    assert_eq!(
        std::fs::canonicalize(initial["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&workspace).unwrap()
    );
    Probe {
        workspace,
        id,
        initial,
    }
}

impl Probe {
    pub(crate) async fn resume(self, window: &mut Window) {
        attach(window).await;
        let pane = &window.app.terminal;
        let tab_id = pane.current.unwrap();
        assert_eq!(
            pane.tabs[&tab_id].terminal_id.as_deref(),
            Some(self.id.as_str())
        );
        type_line(window, "resumed foreground input");
        let result = proof(&self.workspace.join("reply.json")).await;
        for key in ["pid", "parent", "cwd", "variable"] {
            assert_eq!(result[key], self.initial[key], "{key}");
        }
        assert_eq!(result["line"], "resumed foreground input");
        type_line(window, "python -u terminal-probe.py shell");
        let shell = proof(&self.workspace.join("shell.json")).await;
        for key in ["parent", "cwd", "variable"] {
            assert_eq!(shell[key], self.initial[key], "{key}");
        }
        window.dispatch(Message::Terminal(Action::Close(tab_id)));
        window
            .until(|app| !app.terminal.tabs.contains_key(&tab_id))
            .await;
    }
}
