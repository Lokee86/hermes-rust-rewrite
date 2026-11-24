use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use hermes_engine::Config;
use hermes_gateway::{Gateway, GatewayConfig, RunningGateway};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tempfile::tempdir;

#[cfg(windows)]
#[path = "../../engine/test_support/terminal_host.rs"]
mod fixture_host;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
async fn ticket(g: &RunningGateway, scope: &str) -> Result<String> {
    let v = reqwest::Client::new()
        .post(format!(
            "{}/api/webapp/socket-ticket?socket={scope}",
            g.http_url()
        ))
        .bearer_auth(g.token())
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    Ok(v["ticket"].as_str().context("ticket")?.into())
}
async fn connect(g: &RunningGateway, extra: &str) -> Result<Socket> {
    let mut url = reqwest::Url::parse(&g.http_url())?;
    url.set_scheme("ws").unwrap();
    url.set_path("/api/host-terminal");
    url.set_query(Some(extra));
    url.query_pairs_mut()
        .append_pair("ticket", &ticket(g, "host-terminal").await?);
    Ok(connect_async(url.as_str()).await?.0)
}
async fn meta(ws: &mut Socket) -> Result<Value> {
    let msg = tokio::time::timeout(Duration::from_secs(45), ws.next())
        .await?
        .context("no metadata")??;
    let text = msg.into_text()?;
    Ok(serde_json::from_str(
        text.strip_prefix("\0HERMES_TERMINAL_META:")
            .context("meta prefix")?,
    )?)
}
async fn output(ws: &mut Socket, marker: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut buf = Vec::new();
        while let Some(msg) = ws.next().await {
            match msg? {
                Message::Binary(bytes) => {
                    buf.extend_from_slice(&bytes);
                    let s = String::from_utf8_lossy(&buf);
                    if s.contains(marker) {
                        return Ok(s.into_owned());
                    }
                    ensure!(
                        buf.len() < 1 << 20,
                        "terminal did not produce expected response"
                    )
                }
                Message::Close(reason) => anyhow::bail!("terminal closed {reason:?}"),
                _ => {}
            }
        }
        anyhow::bail!("terminal ended")
    })
    .await?
}
async fn gateway(dir: &Path) -> Result<RunningGateway> {
    Gateway::start(GatewayConfig::new(dir, Config::from_env())).await
}
#[tokio::test]
async fn host_ticket_is_scoped_single_use_and_requires_auth() -> Result<()> {
    let dir = tempdir()?;
    let g = gateway(dir.path()).await?;
    let client = reqwest::Client::new();
    let ticket_url = format!(
        "{}/api/webapp/socket-ticket?socket=host-terminal",
        g.http_url()
    );
    assert_eq!(client.post(&ticket_url).send().await?.status(), 401);
    assert_eq!(
        client
            .post(format!("{ticket_url}&profile=other"))
            .bearer_auth(g.token())
            .send()
            .await?
            .status(),
        403
    );
    let t = ticket(&g, "host-terminal").await?;
    assert!(
        connect_async(format!("{}?ticket={t}", g.websocket_url()))
            .await
            .is_err()
    );
    let url = format!(
        "ws://{}/api/host-terminal?ticket={t}&profile=other",
        g.address()
    );
    let (mut ws, _) = connect_async(&url).await?;
    assert!(matches!(ws.next().await,Some(Ok(Message::Close(Some(f)))) if u16::from(f.code)==4403));
    assert!(connect_async(&url).await.is_err());
    let gateway_ticket = ticket(&g, "gateway").await?;
    assert!(
        connect_async(format!(
            "ws://{}/api/host-terminal?ticket={gateway_ticket}",
            g.address()
        ))
        .await
        .is_err()
    );
    // A cancellation may arrive before the request it cancels.
    let key = uuid::Uuid::new_v4().to_string();
    let mut cancel = connect(&g, &format!("launch_key={key}&action=close")).await?;
    let core = hermes_engine::herdr::Core::new(dir.path())?;
    let id = core.shell_launch_id(&key);
    let reply = meta(&mut cancel).await?;
    assert_eq!(reply["terminalId"], id);
    assert_eq!(reply["closed"], true);
    assert!(core.lookup(&id)?.is_none());
    assert!(
        dir.path()
            .join("terminal-host")
            .join(format!("{id}.json"))
            .is_file()
    );
    g.shutdown().await
}
/// Set HERMES_HERDR_CORE to the verified executable from scripts/prepare-herdr.ps1.
#[cfg(windows)]
#[tokio::test]
#[ignore = "requires pinned native Herdr Core 0.1.22 and Python on PATH"]
async fn windows_child_survives_gateway_restart() -> Result<()> {
    let dir = tempdir()?;
    let _fixture_host = fixture_host::FixtureHost::for_profile(dir.path());
    let other = tempdir()?;
    let g = gateway(dir.path()).await?;
    let child = dir.path().join("child.py");
    std::fs::write(
        &child,
        "import os,sys\nprint('CHILD=' + str(os.getpid()) + ' READY',flush=True)\ns=input()\nprint('RESULT=' + str(os.getpid()) + ':' + s,flush=True)\n",
    )?;
    let launch_key = uuid::Uuid::new_v4().to_string();
    let form = reqwest::Url::parse_with_params(
        "http://localhost",
        [
            ("cwd", dir.path().to_string_lossy().as_ref()),
            ("launch_key", launch_key.as_str()),
        ],
    )?;
    // Disconnect without receiving the launch metadata. A retry must adopt the first shell.
    let lost = connect(&g, form.query().unwrap()).await?;
    drop(lost);
    let core = hermes_engine::herdr::Core::new(dir.path())?;
    let reserved = core.shell_launch_id(&launch_key);
    let original_pid = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(t) = core.lookup(&reserved)?
                && let Ok(s) = core.status(&t).await
                && let Some(pid) = s["pid"].as_u64()
            {
                return anyhow::Ok(pid);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    let mut first = connect(&g, form.query().unwrap()).await?;
    let id = meta(&mut first).await?["terminalId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(id, reserved);
    let t = core.lookup(&id)?.context("missing terminal")?;
    let shell_pid = core.status(&t).await?["pid"]
        .as_u64()
        .context("missing shell pid")?;
    assert_eq!(shell_pid, original_pid);
    first.send(Message::Binary(b"cd\r".to_vec().into())).await?;
    let cwd_output = output(&mut first, &dir.path().to_string_lossy()).await?;
    assert!(cwd_output.contains(&dir.path().to_string_lossy().to_string()));
    first
        .send(Message::Text("\x1b[RESIZE:93;31]".into()))
        .await?;
    first
        .send(Message::Binary(
            b"set HERMES_NATIVE_CONTINUITY=persisted\r".to_vec().into(),
        ))
        .await?;
    first
        .send(Message::Binary(b"python -u child.py\r".to_vec().into()))
        .await?;
    let captured = output(&mut first, " READY").await?;
    let pid = captured
        .rsplit("CHILD=")
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .context("missing child pid")?
        .parse::<u32>()?;
    first.close(None).await?;
    drop(first);
    g.shutdown().await?;
    let g = gateway(dir.path()).await.context("restart the original terminal gateway")?;
    let foreign = gateway(other.path()).await.context("start a separate profile gateway")?;
    let mut denied = connect(&foreign, &format!("attach={id}")).await?;
    assert!(
        matches!(denied.next().await,Some(Ok(Message::Close(Some(f)))) if u16::from(f.code)==4410)
    );
    foreign.shutdown().await?;
    let mut resumed = connect(&g, &format!("attach={id}")).await?;
    let m = meta(&mut resumed).await?;
    assert_eq!(m["terminalId"], id);
    assert_eq!(m["reconnected"], true);
    output(&mut resumed, &format!("CHILD={pid}")).await?;
    resumed
        .send(Message::Binary(b"continued\r".to_vec().into()))
        .await?;
    output(&mut resumed, &format!("RESULT={pid}:continued")).await?;
    resumed
        .send(Message::Binary(
            b"echo STATE=%HERMES_NATIVE_CONTINUITY%\r".to_vec().into(),
        ))
        .await?;
    output(&mut resumed, "STATE=persisted").await?;
    assert_eq!(core.status(&t).await?["pid"], shell_pid);
    // Check actual console input and dimensions from inside a real foreground program.
    let probe = r#"import json, msvcrt, os, time
from pathlib import Path
w, h = os.get_terminal_size()
keys = []
try:
    print('CONSOLE-INPUT-READY', flush=True)
    while True:
        if msvcrt.kbhit():
            key = ord(msvcrt.getwch())
            keys.append(key)
            if key in (0, 224):
                keys.append(ord(msvcrt.getwch()))
            if len(keys) >= 3:
                print('CONSOLE-KEYS-RECEIVED', flush=True)
            if 3 in keys:
                break
        time.sleep(0.01)
except KeyboardInterrupt:
    pass
Path('console-result.json').write_text(json.dumps({
    'keys': keys, 'cols': w, 'rows': h,
    'private_env': any(k in os.environ for k in ('HERMES_RUST_API_KEY','HERMES_MCP_TEST_TOKEN'))
}))
print('CONSOLE-INPUT-COMPLETE', flush=True)
"#;
    std::fs::write(dir.path().join("probe.py"), probe)?;
    resumed
        .send(Message::Text("\x1b[RESIZE:104;35]".into()))
        .await?;
    resumed
        .send(Message::Binary(b"python -u probe.py\r".to_vec().into()))
        .await?;
    output(&mut resumed, "CONSOLE-INPUT-READY").await?;
    resumed
        .send(Message::Binary("λ".as_bytes().to_vec().into()))
        .await?;
    resumed
        .send(Message::Binary(b"\x1b[A".to_vec().into()))
        .await?;
    output(&mut resumed, "CONSOLE-KEYS-RECEIVED").await?;
    resumed.send(Message::Binary(vec![3].into())).await?;
    output(&mut resumed, "CONSOLE-INPUT-COMPLETE").await?;
    let probe: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("console-result.json"))?)?;
    assert_eq!(probe["cols"], 104, "{probe}");
    assert_eq!(probe["rows"], 35, "{probe}");
    assert_eq!(probe["private_env"], false, "{probe}");
    let keys: Vec<u64> = serde_json::from_value(probe["keys"].clone())?;
    assert!(keys.contains(&u64::from('λ')), "{probe}");
    assert!(
        keys.windows(2).any(|s| s == [224, 72] || s == [0, 72])
            || keys.windows(3).any(|s| s == [27, 91, 65]),
        "{probe}"
    );
    resumed
        .send(Message::Binary(b"python -u child.py\r".to_vec().into()))
        .await?;
    let s = output(&mut resumed, " READY").await?;
    let ending_pid = s
        .rsplit("CHILD=")
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .context("missing child pid")?
        .parse::<u32>()?;
    let mut ending = connect(&g, &format!("attach={id}&action=close")).await?;
    assert_eq!(meta(&mut ending).await?["closed"], true);
    assert!(
        matches!(ending.next().await,Some(Ok(Message::Close(Some(f)))) if u16::from(f.code)==1000)
    );
    let mut stale_launch = connect(&g, form.query().unwrap()).await?;
    assert!(matches!(
        stale_launch.next().await,
        Some(Ok(Message::Close(_)))
    ));
    assert!(core.lookup(&id)?.is_none());
    let mut expired = connect(&g, &format!("attach={id}")).await?;
    assert!(
        matches!(expired.next().await,Some(Ok(Message::Close(Some(f)))) if u16::from(f.code)==4410)
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let p = tokio::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {ending_pid}"), "/FO", "CSV", "/NH"])
                .output()
                .await
                .unwrap();
            if !String::from_utf8_lossy(&p.stdout).contains(&format!("\"{ending_pid}\"")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await?;
    eprintln!(
        "Verified shell PID {shell_pid}, child PID {pid}, explicit-close child PID {ending_pid}"
    );
    g.shutdown().await
}
