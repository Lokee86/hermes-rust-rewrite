//! Drive the normal Desktop bootstrap, Send, Approve and Close handlers without a display.
use super::*;
use iced::futures::StreamExt;
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};

struct FixtureProcess(std::process::Child);
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) struct Window {
    pub(crate) app: Hermes,
    tx: tokio::sync::mpsc::UnboundedSender<Message>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Window {
    fn new() -> Self {
        let (app, boot) = Hermes::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut window = Self {
            app,
            tx,
            rx,
            tasks: vec![],
        };
        window.schedule(boot);
        window
    }
    pub(crate) fn schedule(&mut self, task: Task<Message>) {
        if let Some(mut stream) = iced_runtime::task::into_stream(task) {
            let tx = self.tx.clone();
            self.tasks.retain(|task| !task.is_finished());
            self.tasks.push(tokio::spawn(async move {
                while let Some(action) = stream.next().await {
                    if let iced_runtime::Action::Output(message) = action
                        && tx.send(message).is_err()
                    {
                        break;
                    }
                }
            }));
        }
    }
    pub(crate) fn dispatch(&mut self, msg: Message) {
        let work = self.app.update(msg);
        self.schedule(work);
    }
    pub(crate) async fn until(&mut self, ready: impl Fn(&Hermes) -> bool) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !ready(&self.app) {
                let message = self.rx.recv().await.expect("window tasks disappeared");
                self.dispatch(message);
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Desktop timed out. Last banner: {:?}",
                self.app.banner.as_ref().map(|b| &b.text)
            )
        });
    }
    fn send(&mut self, text: &str) {
        self.app.composer = text_editor::Content::with_text(text);
        self.dispatch(Message::Send);
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[path = "../../engine/test_support/terminal_host.rs"]
mod fixture_host;

fn test_root() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("HERMES_RUST_DATA_DIR").expect("provide an empty scratch profile"),
    );
    assert!(path.is_absolute() && path.is_dir() && path.read_dir().unwrap().next().is_none());
    path
}

#[test]
fn saved_credentials_precede_chat_bootstrap_and_remote_tabs_keep_their_targets() {
    const CHILD: &str = "HERMES_DESKTOP_ISOLATED_BOOTSTRAP_TEST";
    if std::env::var_os(CHILD).is_none() {
        let isolated = tempfile::tempdir().unwrap();
        let profile = isolated.path().join("profile");
        std::fs::create_dir(&profile).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "interaction_tests::saved_credentials_precede_chat_bootstrap_and_remote_tabs_keep_their_targets", "--test-threads=1", "--nocapture"])
            .env(CHILD, "1").env("HERMES_RUST_DATA_DIR", &profile)
            .env("HERMES_HOME", isolated.path().join("stock-home"))
            .env_remove("HERMES_RUST_BASE_URL").env_remove("HERMES_RUST_API_KEY").env_remove("HERMES_RUST_MODEL")
            .env_remove("OPENROUTER_API_KEY").env_remove("OPENAI_API_KEY").env_remove("ANTHROPIC_API_KEY")
            .output().unwrap();
        assert!(result.status.success(), "isolated bootstrap failed:\n{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        return;
    }
    let root = test_root();
    assert!(std::env::var_os("HERMES_RUST_BASE_URL").is_none());
    assert!(std::env::var_os("HERMES_RUST_API_KEY").is_none());
    assert!(std::env::var_os("HERMES_RUST_MODEL").is_none());
    let config = Config { base_url: hermes_engine::OPENAI.into(), model: "saved-model".into(), api_key: "synthetic-saved-key".into() };
    let vault = hermes_engine::credentials::EndpointStore::open(&root.join("providers")).unwrap();
    vault.save(None, "Saved provider", &config).unwrap();
    Preferences::save(&config).unwrap();
    let (mut app, boot) = Hermes::new();
    assert!(app.chat.has_startup_provider(&config), "the gateway captures the restored credential");
    drop(boot);
    app.chat = chat_view::View::default();
    app.connections.config.mode = connections::Mode::Remote;
    let origin = "http://127.0.0.1:1";
    app.connections.config.url = origin.into();
    let a = remote::remote_summary(&root, origin, &json!({"id":"remote-a","title":"A"})).unwrap();
    let b = remote::remote_summary(&root, origin, &json!({"id":"remote-b","title":"B"})).unwrap();
    app.open_gateway_chat(a);
    app.open_gateway_chat(b);
    app.sessions.clear(); // A search/filter can omit either tab from the sidebar.
    let _ = app.tab_action(tabs::Action::Select("remote-a".into()));
    assert_eq!(app.current.as_ref().unwrap().id, "remote-a");
    let _ = app.tab_action(tabs::Action::Close("remote-a".into()));
    assert_eq!(app.current.as_ref().unwrap().id, "remote-b");
    let _ = app.tab_action(tabs::Action::Reopen);
    assert_eq!(app.current.as_ref().unwrap().id, "remote-a");
    app.connections.config.mode = connections::Mode::Local;
    app.refresh_gateway_route();
    assert!(app.tabs.entries.iter().all(|tab| !tab.id.starts_with("remote-")));
    assert!(app.sessions.iter().all(|session| !session.id.starts_with("remote-")));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a new empty HERMES_RUST_DATA_DIR for the process-wide desktop profile"]
async fn two_desktop_viewers_resume_the_same_tool_turn() {
    use axum::{Json, Router, http::header, routing::post};
    let root = test_root();
    let _fixture_host = fixture_host::FixtureHost::for_profile(&root);
    let workspace = root.join("fixture-project");
    std::fs::create_dir_all(&workspace).unwrap();
    let library = Library::open(&root).unwrap();
    let project = library.add_project(&workspace).unwrap();
    library.select_project(Some(&project.id)).unwrap();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let provider = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/chat/completions",post(|Json(body):Json<Value>| async move {
            let message = body["messages"].as_array().unwrap().last().unwrap();
            if message["content"] == "provider-error" {
                return ([(header::CONTENT_TYPE,"text/event-stream")], "data: {\"error\":{\"message\":\"fixture rejected\"}}\n\n".to_owned());
            }
            let (delta, finish) = if message["role"] == "tool" { (json!({"content":"Clock read."}), "stop") }
                else { (json!({"tool_calls":[{"index":0,"id":"clock","function":{"name":"get_current_time","arguments":"{}"}}]}), "tool_calls") };
            let frame = json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
            ([(header::CONTENT_TYPE,"text/event-stream")], format!("data: {frame}\n\ndata: [DONE]\n\n"))
        }))).await.unwrap();
    });
    let mut changes = serde_json::Map::new();
    changes.insert("approval_mode".into(), json!("manual"));
    hermes_engine::settings::ProfileSettings::update(&root, &changes).unwrap();
    let config = Config {
        base_url: format!("http://{addr}/v1"),
        model: "fixture-model".into(),
        api_key: String::new(),
    };
    let mut child = None;
    let mut remote_owner = None;
    if let Some(exe) = std::env::var_os("HERMES_TEST_DESKTOP_EXE") {
        use std::process::{Command, Stdio};
        Preferences::save(&config).unwrap();
        let mut command = Command::new(exe);
        command
            .args([
                "gateway",
                "run",
                "--ephemeral-token",
                "--port",
                "0",
                "--data-dir",
            ])
            .arg(&root)
            .env("HERMES_RUST_DATA_DIR", &root)
            .env("HERMES_RUST_BASE_URL", &config.base_url)
            .env("HERMES_RUST_MODEL", &config.model)
            .env("HERMES_RUST_API_KEY", "")
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        child = Some(FixtureProcess(
            command
                .spawn()
                .expect("launch installed desktop executable as gateway"),
        ));
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if hermes_gateway::owner::discover(&root)
                    .await
                    .unwrap()
                    .is_some()
                {
                    break;
                }
                assert!(
                    child.as_mut().unwrap().0.try_wait().unwrap().is_none(),
                    "headless owner exited during startup"
                );
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await
        .expect("independent owner did not publish a control endpoint");
    } else {
        let gateway =
            hermes_gateway::Gateway::start(hermes_gateway::GatewayConfig::new(&root, config))
                .await
                .unwrap();
        std::fs::write(
            root.join("connection.json"),
            json!({"mode":"remote","url":gateway.http_url()}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("connection-token"), gateway.token()).unwrap();
        remote_owner = Some(gateway);
    }
    let mut a = Window::new();
    a.until(|app| app.chat.ready).await;
    #[cfg(windows)]
    let terminal = if std::env::var_os("HERMES_TEST_INCLUDE_TERMINAL").is_some() {
        assert!(
            child.is_some(),
            "joint test requires HERMES_TEST_DESKTOP_EXE"
        );
        Some(crate::terminal::acceptance::open(&mut a).await)
    } else {
        None
    };
    let sid = a.app.current.as_ref().unwrap().id.clone();
    let mut b = Window::new();
    // Make selection explicit independently of the user's reopen-last-chat display preference.
    let selected = a.app.current.clone().unwrap();
    b.app.open_chat(selected);
    let attach = Task::batch(std::mem::take(&mut b.app.chat.tasks));
    b.schedule(attach);
    b.until(|app| app.chat.ready && app.current.as_ref().is_some_and(|s| s.id == sid))
        .await;
    a.send("clock");
    a.until(|app| !app.approvals.is_empty()).await;
    a.dispatch(Message::Close);
    drop(a);
    b.until(|app| !app.approvals.is_empty()).await;
    let prompt = &b.app.approvals[0];
    b.dispatch(Message::Approve(
        prompt.call.id.clone(),
        prompt.generation,
        true,
    ));
    b.until(|app| {
        !app.running
            && app.history.turns.last().is_some_and(|t| {
                t.entries
                    .iter()
                    .any(|e| matches!(e, Entry::Assistant {text,..} if text == "Clock read."))
            })
    })
    .await;
    assert_eq!(b.app.history.turns.len(), 1);
    assert_eq!(
        b.app.history.turns[0]
            .entries
            .iter()
            .filter(|e| matches!(e, Entry::Tool { .. }))
            .count(),
        1
    );
    let expected = b.app.history.clone();
    let summary = b.app.current.clone().unwrap();
    b.dispatch(Message::Close);
    drop(b);
    let mut reopened = Window::new();
    reopened.app.open_chat(summary);
    let attach = Task::batch(std::mem::take(&mut reopened.app.chat.tasks));
    reopened.schedule(attach);
    reopened
        .until(|app| app.chat.ready && app.history == expected)
        .await;
    #[cfg(windows)]
    if let Some(terminal) = terminal {
        terminal.resume(&mut reopened).await;
    }
    reopened.send("provider-error");
    reopened
        .until(|app| {
            !app.running
                && app
                    .history
                    .turns
                    .last()
                    .is_some_and(|t| matches!(t.status, hermes_engine::Status::Failed(_)))
        })
        .await;
    reopened.send("clock");
    reopened
        .until(|app| app.running && !app.approvals.is_empty())
        .await;
    reopened.dispatch(Message::Stop);
    reopened
        .until(|app| {
            !app.running
                && app
                    .history
                    .turns
                    .last()
                    .is_some_and(|t| t.status == hermes_engine::Status::Interrupted)
        })
        .await;
    drop(reopened);
    if let Some(gateway) = remote_owner {
        gateway.shutdown().await.unwrap();
    }
    if child.is_some() {
        hermes_gateway::owner::discover(&root)
            .await
            .unwrap()
            .unwrap()
            .stop()
            .await
            .unwrap();
    }
    provider.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Nous Portal OAuth, a running local owner, and a new temporary profile"]
async fn nous_qwen_send_approve_stream_and_reopen() {
    let _ = test_root();
    Preferences::save(&Config {
        base_url: portal::INFERENCE.into(),
        model: "qwen/qwen4-max:free".into(),
        api_key: String::new(),
    })
    .unwrap();
    let mut a = Window::new();
    a.until(|app| app.chat.ready).await;
    a.send("Call get_current_time exactly once with an empty arguments object. Then report its exact UTC timestamp.");
    let mut granted = HashSet::new();
    loop {
        a.until(|app| {
            app.approvals.iter().any(|p| !granted.contains(&p.call.id))
                || (!app.running
                    && app
                        .history
                        .turns
                        .last()
                        .is_some_and(|t| t.status != hermes_engine::Status::Running))
        })
        .await;
        let Some(card) = a
            .app
            .approvals
            .iter()
            .find(|p| !granted.contains(&p.call.id))
        else {
            break;
        };
        assert_eq!(card.call.name, "get_current_time");
        granted.insert(card.call.id.clone());
        a.dispatch(Message::Approve(
            card.call.id.clone(),
            card.generation,
            true,
        ));
    }
    assert_eq!(granted.len(), 1);
    let expected = a.app.history.clone();
    assert_eq!(
        expected.turns.last().unwrap().status,
        hermes_engine::Status::Complete
    );
    let summary = a.app.current.clone().unwrap();
    a.dispatch(Message::Close);
    drop(a);
    let mut reopened = Window::new();
    reopened.app.open_chat(summary);
    let attach = Task::batch(std::mem::take(&mut reopened.app.chat.tasks));
    reopened.schedule(attach);
    reopened
        .until(|app| app.chat.ready && app.history == expected)
        .await;
}
