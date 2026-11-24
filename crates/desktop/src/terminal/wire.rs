//! A native viewer of Hermes' authenticated /api/host-terminal socket.
use super::{Action, Bookmark, Scope};
use hermes_engine::Config;
use iced::futures::{SinkExt, StreamExt, channel::mpsc::Sender};
use serde::Deserialize;
use std::{path::PathBuf, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

#[derive(Clone)]
pub(super) enum Access {
    Local {
        root: PathBuf,
        provider: Config,
    },
    Remote {
        root: PathBuf,
        url: String,
        token: String,
    },
}

const EXPIRED: &str = "Terminal exited or expired";
const WAIT: Duration = Duration::from_secs(15);
const META: &str = "\0HERMES_TERMINAL_META:";
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    terminal_id: Option<String>,
    #[serde(default)]
    shell: String,
    #[serde(default)]
    closed: bool,
}

pub(super) enum Command {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    End,
}

fn origin(value: &str) -> Result<url::Url, String> {
    let base = url::Url::parse(value).map_err(|_| "Invalid terminal gateway address")?;
    let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(base.scheme() == "https" || (base.scheme() == "http" && loopback))
        || base.host().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("Use HTTPS or a local loopback gateway for the terminal.".into());
    }
    Ok(base)
}

impl Access {
    async fn ticket(&self) -> Result<(url::Url, String), String> {
        match self {
            Self::Local { root, provider } => {
                let owner = hermes_gateway::owner::ensure_local(root, provider)
                    .await
                    .map_err(|e| format!("Could not reach the profile gateway: {e}"))?;
                let (http, ticket) = owner.terminal_ticket().await.map_err(|e| e.to_string())?;
                Ok((origin(&http)?, ticket))
            }
            Self::Remote { root, url, token } => {
                let base = origin(url)?;
                let mut endpoint = base.clone();
                endpoint.set_path(&format!(
                    "{}/api/webapp/socket-ticket",
                    base.path().trim_end_matches('/')
                ));
                endpoint
                    .query_pairs_mut()
                    .append_pair("socket", "host-terminal");
                let token = if token.is_empty() {
                    crate::gateway_oauth::access_token(root, url).await?
                } else {
                    token.clone()
                };
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(WAIT)
                    .build()
                    .map_err(|_| "Could not initialize terminal authentication")?;
                let response = client
                    .post(endpoint)
                    .header("X-Hermes-Session-Token", token)
                    .send()
                    .await
                    .map_err(|_| "Terminal ticket service unavailable")?;
                if !response.status().is_success() {
                    return Err(format!("Terminal ticket refused ({})", response.status()));
                }
                #[derive(Deserialize)]
                struct Ticket {
                    ticket: String,
                }
                let reply = response
                    .json::<Ticket>()
                    .await
                    .map_err(|_| "Invalid terminal ticket response")?;
                if reply.ticket.is_empty() {
                    return Err("The terminal ticket is empty".into());
                }
                Ok((base, reply.ticket))
            }
        }
    }
}

pub(super) struct Request {
    pub access: Access,
    pub scope: Scope,
    pub bookmark: PathBuf,
    pub attach: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

impl Request {
    async fn open(&self, close: bool) -> Result<(Socket, Metadata), String> {
        let (mut url, ticket) = self.access.ticket().await?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| "Invalid terminal transport")?;
        url.set_path(&format!(
            "{}/api/host-terminal",
            url.path().trim_end_matches('/')
        ));
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("ticket", &ticket);
            if let Some(id) = &self.attach {
                query.append_pair("attach", id);
            } else {
                if matches!(self.access, Access::Local { .. }) {
                    let key = self
                        .bookmark
                        .file_stem()
                        .and_then(|v| v.to_str())
                        .and_then(|v| uuid::Uuid::parse_str(v).ok())
                        .ok_or("Invalid terminal launch key")?;
                    query.append_pair("launch_key", &key.to_string());
                }
                if let Some(cwd) = &self.scope.workspace {
                    query.append_pair("cwd", &cwd.to_string_lossy());
                }
            }
            query.append_pair("cols", &self.cols.to_string());
            query.append_pair("rows", &self.rows.to_string());
            if close {
                query.append_pair("action", "close");
            }
        }
        // Errors from the WebSocket library can contain the ticketed URL.
        let (mut socket, _) = tokio::time::timeout(WAIT, connect_async(url.as_str()))
            .await
            .map_err(|_| "Terminal connection timed out")?
            .map_err(|_| "Terminal socket unavailable")?;
        let metadata = tokio::time::timeout(WAIT, async {
            while let Some(frame) = socket.next().await {
                match frame.map_err(|_| "Terminal connection lost during attach")? {
                    Message::Text(text) => {
                        return serde_json::from_str::<Metadata>(
                            text.strip_prefix(META).ok_or("Terminal metadata missing")?,
                        )
                        .map_err(|_| "Invalid terminal metadata".to_owned());
                    }
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(frame) => return Err(close_reason(frame.as_ref())),
                    _ => return Err("Unexpected data before terminal metadata".into()),
                }
            }
            Err("Terminal disconnected before attachment".into())
        })
        .await
        .map_err(|_| "Timed out waiting for terminal metadata")??;
        Ok((socket, metadata))
    }

    pub async fn end(&self) -> Result<(), String> {
        if self.attach.is_none() && !matches!(self.access, Access::Local { .. }) {
            return Err("Missing terminal ID".into());
        }
        match self.open(true).await {
            Ok((mut socket, response)) => {
                let _ = socket.close(None).await;
                if !response.closed {
                    return Err("Gateway did not confirm terminal close".into());
                }
            }
            Err(e) if e == EXPIRED => {}
            Err(e) => return Err(e),
        }
        match std::fs::remove_file(&self.bookmark) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!(
                "Terminal ended, but its bookmark could not be removed: {e}"
            )),
        }
    }

    pub async fn stream(
        mut self,
        tab: u64,
        generation: u64,
        input: async_channel::Receiver<Command>,
        mut ui: Sender<Action>,
    ) {
        let result = self.pump(tab, generation, input, &mut ui).await;
        let _ = ui
            .send(Action::Disconnected(tab, generation, result.err()))
            .await;
    }

    async fn pump(
        &mut self,
        tab: u64,
        gen_id: u64,
        input: async_channel::Receiver<Command>,
        ui: &mut Sender<Action>,
    ) -> Result<(), String> {
        if input.is_closed() {
            return Ok(());
        }
        let (mut socket, metadata) = self.open(false).await?;
        let id = metadata
            .terminal_id
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .ok_or("Terminal ID missing from gateway reply")?;
        if self.attach.as_ref().is_some_and(|expected| *expected != id) {
            return Err("Gateway returned a different terminal".into());
        }
        let created = self.attach.is_none();
        self.attach = Some(id.clone());
        if created {
            let record = Bookmark {
                scope: self.scope.clone(),
                terminal_id: Some(id.clone()),
                name: metadata.shell.clone(),
            };
            let path = self.bookmark.clone();
            let saved = tokio::task::spawn_blocking(move || record.save(&path))
                .await
                .map_err(|e| e.to_string())?;
            if let Err(e) = saved {
                let cleanup = self.end().await;
                return Err(match cleanup {
                    Ok(()) => format!("Could not save terminal for reattachment: {e}"),
                    Err(error) => {
                        format!("Could not save terminal or confirm its closure: {e}; {error}")
                    }
                });
            }
        }
        if ui
            .send(Action::Attached(tab, gen_id, id, metadata.shell))
            .await
            .is_err()
        {
            return Ok(());
        }
        loop {
            tokio::select! {
                command = input.recv() => match command {
                    Ok(Command::Input(bytes)) => socket.send(Message::Binary(bytes.into())).await
                        .map_err(|_| "Could not send terminal input")?,
                    Ok(Command::Resize { cols, rows }) => socket.send(Message::Text(
                        format!("\x1b[RESIZE:{cols};{rows}]").into())).await.map_err(|_| "Could not resize terminal")?,
                    Ok(Command::End) => {
                        self.end().await?;
                        let _ = ui.send(Action::Closed(tab, gen_id, Ok(()))).await;
                        break;
                    }
                    Err(_) => break,
                },
                frame = socket.next() => match frame {
                    Some(Ok(Message::Binary(data))) => {
                        if ui.send(Action::Output(tab, gen_id, data.to_vec().into())).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(frame))) => return Err(close_reason(frame.as_ref())),
                    Some(Ok(Message::Ping(_))) => socket.flush().await.map_err(|_| "Terminal disconnected")?,
                    Some(Ok(_)) => {},
                    Some(Err(_)) | None => return Err("Terminal connection lost".into()),
                }
            }
        }
        let _ = socket.close(None).await;
        Ok(())
    }
}

fn close_reason(frame: Option<&tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> String {
    match frame.map(|f| u16::from(f.code)) {
        Some(4410) => EXPIRED.into(),
        Some(4409) => "Terminal attached in another window".into(),
        Some(4401 | 4403) => "Terminal access denied".into(),
        _ => "Terminal detached".into(),
    }
}
