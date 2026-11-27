//! The Hermes Webapp host-terminal wire. Each listener authenticates a single profile owner.
use super::*;
use axum::extract::ws::CloseFrame;
use hermes_engine::herdr::{Core, Frame, Terminal};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken as Cancel;

const META: &str = "\0HERMES_TERMINAL_META:";
pub(super) struct Host {
    tickets: auth::Tickets,
    // The most recent attachment owns the input stream.
    viewers: StdMutex<HashMap<String, (String, Cancel)>>,
    admission: Mutex<()>,
}
impl Host {
    pub(super) fn new() -> Self {
        Self {
            tickets: auth::Tickets::default(),
            viewers: StdMutex::new(HashMap::new()),
            admission: Mutex::new(()),
        }
    }
}
#[derive(Deserialize)]
pub(super) struct TicketRequest {
    socket: String,
    profile: Option<String>,
}
pub(super) async fn ticket(
    access: auth::Access,
    State(shared): State<Arc<Shared>>,
    Query(req): Query<TicketRequest>,
) -> Response {
    if !rest::profile_matches(&shared, req.profile.as_deref(), false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let tickets = match req.socket.as_str() {
        "gateway" => &shared.tickets,
        "host-terminal" => &shared.terminal.tickets,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    tickets.mint(access)
}
#[derive(Deserialize)]
pub(super) struct Request {
    ticket: String,
    profile: Option<String>,
    attach: Option<String>,
    launch_key: Option<String>,
    action: Option<String>,
    cwd: Option<PathBuf>,
    cols: Option<u16>,
    rows: Option<u16>,
}
pub(super) async fn upgrade(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(req): Query<Request>,
    ws: WebSocketUpgrade,
) -> Response {
    let audience = match shared.auth.audience(&shared, &headers, peer) {
        Ok(audience) => audience,
        Err(status) => return status.into_response(),
    };
    let Some(ticket) = shared.terminal.tickets.redeem(&req.ticket, audience) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let handler = shared.gateway_tasks.token();
    ws.max_frame_size(1 << 20)
        .max_message_size(1 << 20)
        .on_upgrade(move |socket| async move {
            serve(shared, socket, req, ticket.revocation()).await;
            drop(handler);
        })
}
/// Bounded: a viewer that stopped reading must not hold the handler, and so gateway shutdown, open.
async fn close(ws: &mut WebSocket, code: u16, reason: &'static str) {
    let frame = Message::Close(Some(CloseFrame { code, reason: reason.into() }));
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, ws.send(frame)).await;
}
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
async fn meta(ws: &mut WebSocket, value: Value) -> bool {
    ws.send(Message::Text(format!("{META}{value}").into()))
        .await
        .is_ok()
}
async fn serve(shared: Arc<Shared>, mut ws: WebSocket, req: Request, revoked: CancellationToken) {
    if revoked.is_cancelled() || !rest::profile_matches(&shared, req.profile.as_deref(), false)
        || (req.attach.is_some() && req.launch_key.is_some())
        || req
            .action
            .as_deref()
            .is_some_and(|a| a != "close" || (req.attach.is_none() && req.launch_key.is_none()))
    {
        close(&mut ws, 4403, "Terminal access denied").await;
        return;
    }
    let core = match Core::new(shared.library.root()) {
        Ok(c) => c,
        Err(_) => {
            close(&mut ws, 1011, "Terminal host unavailable").await;
            return;
        }
    };
    let cols = req.cols.unwrap_or(80).clamp(1, 2000);
    let rows = req.rows.unwrap_or(24).clamp(1, 1000);
    let admission = shared.terminal.admission.lock().await;
    let key = match req
        .launch_key
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
    {
        Ok(key) => key.map(|v| v.to_string()),
        Err(_) => {
            close(&mut ws, 4400, "Invalid terminal launch key").await;
            return;
        }
    };
    let pending_close = key
        .as_deref()
        .filter(|_| req.action.is_some())
        .map(|k| core.shell_launch_id(k));
    let t = if let Some(id) = req.attach.as_ref().or(pending_close.as_ref()) {
        if !hermes_engine::herdr::valid_id(id) {
            close(
                &mut ws,
                4410,
                "Terminal or profile expired; open a new terminal",
            )
            .await;
            return;
        }
        match core.lookup(id) {
            Ok(Some(t)) => t,
            Ok(None) if req.action.is_some() => {
                if let Some(key) = key.as_deref()
                    && core.cancel_unseen_shell(key).is_err()
                {
                    close(&mut ws, 1011, "Could not record terminal closure").await;
                    return;
                }
                drop(admission);
                meta(&mut ws, json!({"terminalId":id,"closed":true})).await;
                close(&mut ws, 1000, "Closed").await;
                return;
            }
            Ok(None) => {
                close(
                    &mut ws,
                    4410,
                    "Terminal or profile expired; open a new terminal",
                )
                .await;
                return;
            }
            Err(_) => {
                close(&mut ws, 1011, "Could not load terminal").await;
                return;
            }
        }
    } else {
        let cwd = req
            .cwd
            .clone()
            .or_else(std::env::home_dir)
            .unwrap_or_else(|| shared.library.root().to_owned());
        match core.launch_shell(&cwd, cols, rows, key.as_deref()).await {
            Ok(t) => t,
            Err(err) => {
                eprintln!("Terminal launch failed: {err:#}");
                close(&mut ws, 1011, "Could not launch host terminal").await;
                return;
            }
        }
    };
    if req.action.is_some() {
        if core.kill(&t).await.is_err() {
            close(&mut ws, 1011, "Could not confirm terminal completion").await;
            return;
        }
        if let Some((_, previous)) = lock(&shared.terminal.viewers).remove(&t.id) {
            previous.cancel()
        }
        drop(admission);
        meta(&mut ws, json!({"terminalId":t.id,"closed":true})).await;
        close(&mut ws, 1000, "Closed").await;
        return;
    }
    let attach = match core.attach(&t, cols, rows).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Terminal attach failed: {e:#}");
            close(
                &mut ws,
                4410,
                "Terminal or profile expired; open a new terminal",
            )
            .await;
            return;
        }
    };
    let owner = new_id();
    let stop = shared.shutdown.child_token();
    if let Some((_, prev)) =
        lock(&shared.terminal.viewers).insert(t.id.clone(), (owner.clone(), stop.clone()))
    {
        prev.cancel()
    }
    drop(admission);
    if meta(&mut ws,json!({"terminalId":t.id,"shell":t.shell,"cwd":t.cwd,"reconnected":req.attach.is_some(),"retentionSeconds":900,"truncated":req.attach.is_some()})).await{
        pump(&mut ws,&core,&shared,&t,attach,(stop,revoked),&owner).await;
    }
    let mut viewers = lock(&shared.terminal.viewers);
    if viewers.get(&t.id).is_some_and(|(v, _)| v == &owner) {
        viewers.remove(&t.id);
    }
}
fn size(data: &[u8]) -> Option<(u16, u16)> {
    let value =
        std::str::from_utf8(data.strip_prefix(b"\x1b[RESIZE:")?.strip_suffix(b"]")?).ok()?;
    let (c, r) = value.split_once(';')?;
    Some((
        c.parse::<u16>().ok()?.clamp(1, 2000),
        r.parse::<u16>().ok()?.clamp(1, 1000),
    ))
}
async fn pump(
    ws: &mut WebSocket,
    core: &Core,
    shared: &Shared,
    t: &Terminal,
    attach: hermes_engine::herdr::Attach,
    (stop, revoked): (Cancel, CancellationToken),
    owner: &str,
) {
    let (mut output, mut input) = attach.split();
    let (tx, mut rx) = mpsc::channel(4);
    let reader = tokio::spawn(async move {
        loop {
            let frame = output.next().await;
            let done = !matches!(frame, Ok(Frame::Bytes(_)));
            if tx.send(frame).await.is_err() || done {
                break;
            }
        }
    });
    let (code, reason) = loop {
        tokio::select! {
            biased;
            _=revoked.cancelled()=>break (4401,"Signed out"),
            _=stop.cancelled()=>break (4409,"Terminal viewer superseded"),
            frame=rx.recv()=>match frame{
                Some(Ok(Frame::Bytes(data)))=>{let n=data.len();if !matches!(tokio::time::timeout(Duration::from_secs(10),ws.send(Message::Binary(data.into()))).await,Ok(Ok(())))||input.credit(n).await.is_err(){break(1001,"Terminal viewer disconnected")}},
                Some(Ok(Frame::Closed(_)))=>break (4410,"Process exited"),
                _=>break (1011,"Terminal host disconnected")
            },
            message=ws.recv()=>{
                let bytes=match message{Some(Ok(Message::Binary(data)))=>data,Some(Ok(Message::Text(data)))=>data.into(),Some(Ok(Message::Ping(_)|Message::Pong(_)))=>continue,_=>break(1000,"Detached")};
                if !lock(&shared.terminal.viewers).get(&t.id).is_some_and(|(v,_)|v==owner) || !core.owner_live(){break(4403,"Terminal access denied")}
                let result=if let Some((cols,rows))=size(&bytes){input.resize(cols,rows).await}else{input.send(&bytes).await};
                if result.is_err(){break(1011,"Terminal input failed")}
            }
        }
    };
    reader.abort();
    let _ = reader.await;
    close(ws, code, reason).await;
}
