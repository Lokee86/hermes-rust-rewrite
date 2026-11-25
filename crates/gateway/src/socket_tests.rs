use super::{Config, Gateway, GatewayConfig, PROTOCOL, TICKET_PREFIX};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{fs, time::Duration};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

fn request(id: u64, method: &str, params: Value) -> Message {
    Message::Text(
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            .to_string()
            .into(),
    )
}

fn response(frame: Message) -> Value {
    match frame {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected JSON-RPC response, got {other:?}"),
    }
}

#[tokio::test]
async fn closing_a_stalled_writer_has_a_deadline() {
    use std::{
        convert::Infallible,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    struct StalledWriter(AtomicUsize);
    impl futures_util::Sink<()> for StalledWriter {
        type Error = Infallible;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> { Poll::Ready(Ok(())) }
        fn start_send(self: Pin<&mut Self>, _: ()) -> Result<(), Self::Error> { Ok(()) }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> { Poll::Ready(Ok(())) }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Poll::Pending
        }
    }

    let mut writer = StalledWriter(AtomicUsize::new(0));
    timeout(super::SOCKET_CLOSE_TIMEOUT + Duration::from_secs(1), super::close_websocket(&mut writer))
        .await.expect("a peer that stops reading must not stall teardown");
    assert!(writer.0.load(Ordering::Relaxed) > 0);
}

#[tokio::test]
async fn keepalives_are_serviced_during_ordered_shell_capture() {
    let profile = tempfile::tempdir().unwrap();
    let cwd = profile.path().join("workspace");
    fs::create_dir(&cwd).unwrap();
    #[cfg(windows)]
    let capture = {
        if !std::process::Command::new("python.exe")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        fs::write(
            cwd.join("capture.py"),
            "from pathlib import Path\nimport time\nPath('started').touch()\nfor _ in range(400):\n    if Path('release').exists():\n        break\n    time.sleep(0.02)\nprint('complete')\n",
        )
        .unwrap();
        "python.exe capture.py"
    };
    #[cfg(unix)]
    let capture = "echo ready > started; sleep 5; echo complete";

    let gateway = Gateway::start(GatewayConfig::new(profile.path(), Config::from_env()))
        .await
        .unwrap();
    let ticket = gateway.shared.issue_ticket().unwrap();
    let mut upgrade = format!("{}?native_dial=1", gateway.websocket_url())
        .into_client_request()
        .unwrap();
    upgrade.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(&format!("{PROTOCOL}, {TICKET_PREFIX}{ticket}")).unwrap(),
    );
    let (mut socket, _) = connect_async(upgrade).await.unwrap();
    let ready = response(socket.next().await.unwrap().unwrap());
    assert_eq!(ready["params"]["type"], "gateway.ready");

    socket
        .send(request(
            1,
            "session.create",
            json!({"source":"tui","cwd":cwd}),
        ))
        .await
        .unwrap();
    let session = response(socket.next().await.unwrap().unwrap());
    let id = session["result"]["session_id"].as_str().unwrap();
    socket
        .send(request(
            2,
            "shell.exec",
            json!({"session_id":id,"command":capture}),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !cwd.join("started").is_file() {
        assert!(Instant::now() < deadline, "shell capture never started");
        sleep(Duration::from_millis(10)).await;
    }
    socket
        .send(request(3, "runtime.describe", json!({})))
        .await
        .unwrap();
    let wire_ping = b"shell keepalive";
    socket
        .send(Message::Ping(wire_ping.to_vec().into()))
        .await
        .unwrap();
    let pong = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("WebSocket ping stalled behind shell capture")
        .unwrap()
        .unwrap();
    assert_eq!(pong, Message::Pong(wire_ping.to_vec().into()));

    socket
        .send(request(4, "gateway.ping", json!({})))
        .await
        .unwrap();
    let pong = response(
        timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("gateway.ping stalled behind shell capture")
            .unwrap()
            .unwrap(),
    );
    assert_eq!(pong["id"], 4);
    assert_eq!(pong["result"]["ok"], true);

    // Exceed the pending request capacity while capture is still ahead of the burst.
    for id in 5..=16 {
        socket
            .send(request(id, "runtime.describe", json!({})))
            .await
            .unwrap();
    }

    #[cfg(windows)]
    fs::write(cwd.join("release"), "").unwrap();
    timeout(Duration::from_secs(8), async {
        let capture = response(socket.next().await.unwrap().unwrap());
        assert_eq!(capture["id"], 2);
        assert_eq!(
            capture["result"]["stdout"].as_str().unwrap().trim(),
            "complete"
        );
        for id in std::iter::once(3).chain(5..=16) {
            let describe = response(socket.next().await.unwrap().unwrap());
            assert_eq!(describe["id"], id);
            assert!(describe["result"]["profile_id"].is_string());
        }
    })
    .await
    .unwrap();
    socket.close(None).await.unwrap();
    gateway.shutdown().await.unwrap();
}
