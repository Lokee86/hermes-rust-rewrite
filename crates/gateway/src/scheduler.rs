//! Gateway-owned scheduler. Durable claim, authority admission and publication
//! are separately reconciled by the run identity after an owner restart.
use super::*;
use hermes_engine::cron::{self, Claim, Outcome, RunStatus, Store};
use std::collections::{BTreeMap, HashSet};
use tokio::sync::mpsc;

pub(super) fn start(shared: Arc<Shared>, store: Store) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let mut active = HashSet::new();
        let mut workers: Vec<JoinHandle<()>> = Vec::new();
        loop {
            let claims = tokio::select! {
                _ = shared.shutdown.cancelled() => break,
                Some(id) = done_rx.recv() => { active.remove(&id); continue; },
                _ = shared.scheduler_notify.notified() => Vec::new(),
                _ = interval.tick() => {
                    let mut claims = match store.gateway_claims() { Ok(claims) => claims, Err(error) => { eprintln!("scheduler recovery: {error:#}"); vec![] } };
                    if shared.provider.validate().is_ok() {
                        match store.claim_due(chrono::Utc::now()) { Ok(due) => claims.extend(due), Err(error) => eprintln!("scheduler claim: {error:#}") }
                    }
                    claims
                }
            };
            let mut claims = claims;
            if claims.is_empty() { claims = store.gateway_claims().unwrap_or_default(); }
            for claim in claims {
                if !active.insert(claim.run.id.clone()) { continue; }
                let shared = shared.clone(); let store = store.clone(); let done = done_tx.clone();
                workers.retain(|worker| !worker.is_finished());
                workers.push(tokio::spawn(async move {
                    let id = claim.run.id.clone();
                    if let Err(error) = run(shared, store, claim).await { eprintln!("scheduler {id}: {error:#}"); }
                    let _ = done.send(id);
                }));
            }
        }
        for worker in workers { let _ = worker.await; }
    })
}

async fn session_for(shared: &Arc<Shared>, store: &Store, claim: &Claim, workspace: &Path, model: &str) -> Result<Arc<LiveSession>> {
    let _guard = shared.creates.lock().await;
    let id = if let Some(id) = &claim.run.gateway_session { id.clone() } else {
        let summary = shared.library.create_session(None, None)?;
        shared.library.rename_session(&summary.id, &format!("Run · {}", claim.job.title()))?;
        let settings = Settings { source: "scheduled".into(), cwd: Some(workspace.to_owned()), model: Some(model.to_owned()), ..Default::default() };
        settings.save(&summary.directory)?;
        store.bind_gateway_session(&claim.run.id, &summary.id)?;
        summary.id
    };
    shared.session(&id).await.map_err(|error| anyhow::anyhow!(error.message))
}

pub(super) async fn routine_for(shared: &Arc<Shared>, job_id: &str, title: &str) -> Result<Arc<LiveSession>> {
    let _guard = shared.creates.lock().await;
    let path = shared.library.root().join("routine-chats.json");
    let mut index: BTreeMap<String, String> = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(error.into()),
    };
    let summary = match index.get(job_id).and_then(|id| shared.library.session(id).ok()).filter(|s| !s.archived) {
        Some(summary) => summary,
        None => {
            let summary = shared.library.create_session(None, None)?;
            let summary = shared.library.rename_session(&summary.id, &format!("Routine · {title}"))?;
            index.insert(job_id.to_owned(), summary.id.clone());
            let mut temp = NamedTempFile::new_in(shared.library.root())?;
            serde_json::to_writer_pretty(&mut temp, &index)?;
            temp.as_file().sync_all()?;
            temp.persist(&path)?;
            summary
        }
    };
    shared.session(&summary.id).await.map_err(|error| anyhow::anyhow!(error.message))
}

async fn run(shared: Arc<Shared>, store: Store, claim: Claim) -> Result<()> {
    let submission = format!("cron:{}", claim.run.id);
    // Once admitted, the durable receipt is authoritative even if the job or
    // provider configuration has changed before recovery.
    let existing = if let Some(id) = &claim.run.gateway_session {
        let session = shared.session(id).await.map_err(|error| anyhow::anyhow!(error.message))?;
        let admitted = shared.submission_receipt(&session, &submission).is_some();
        Some((session, admitted))
    } else { None };
    let session = if let Some((session, true)) = &existing {
        session.clone()
    } else {
        let mut config = shared.provider.clone();
        let workspace = match cron::prepare_job(&store, &claim, &mut config) {
            Ok(workspace) => workspace,
            Err(error) => { store.finish(&claim, Outcome::Failed { error: format!("{error:#}") }, chrono::Utc::now())?; return Ok(()); }
        };
        if store.cancellation_requested(&claim.run.id)? {
            store.finish(&claim, Outcome::Cancelled { reason: "Stopped by the user".into() }, chrono::Utc::now())?;
            return Ok(());
        }
        let session = if let Some((session, _)) = existing { session } else { session_for(&shared, &store, &claim, &workspace, &config.model).await? };
        if claim.run.status == RunStatus::Claimed { store.begin(&claim, chrono::Utc::now())?; }
        shared.submit_background(session.clone(), claim.job.prompt.clone(), submission.clone(), !claim.run.manual).await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        session
    };
    let turn = loop {
        if shared.shutdown.is_cancelled() { return Ok(()); }
        if store.cancellation_requested(&claim.run.id).unwrap_or(true) {
            if let Some(cancel) = &lock(&session.view).cancel { cancel.cancel(); }
        }
        let state = {
            let view = lock(&session.view);
            view.authority.receipts.iter().find(|r| r.input_id.as_deref() == Some(&submission)).map(|r| {
                (r.status.clone(), r.turn_index.and_then(|i| view.history.turns.get(i).cloned()))
            })
        };
        if let Some((status, turn)) = state {
            if status == "terminal" { break turn; }
            // An uncertain external effect is deliberately not rerun or settled.
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    if let Some(turn) = &turn {
        let routine = routine_for(&shared, &claim.job.id, claim.job.title()).await?;
        loop {
            if shared.shutdown.is_cancelled() { return Ok(()); }
            if shared.publish_background_turn(&routine, &format!("cron:{}", claim.run.id), turn.clone()).await? {
                shared.emit(&routine, "session.info", shared.info(&routine));
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let outcome = match turn {
        Some(turn) => match turn.status {
            hermes_engine::Status::Complete => Outcome::Completed { output: turn.entries.iter().filter_map(|entry| if let hermes_engine::Entry::Assistant { text, .. } = entry { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\\n") },
            hermes_engine::Status::Interrupted => Outcome::Cancelled { reason: "Stopped by the user".into() },
            hermes_engine::Status::Failed(error) => Outcome::Failed { error },
            hermes_engine::Status::Running => Outcome::Failed { error: "Run ended without a terminal status".into() },
        },
        None => Outcome::Failed { error: "Run ended without a conversation turn".into() },
    };
    store.finish(&claim, outcome, chrono::Utc::now())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, body::{Body, Bytes}, http::header, response::{IntoResponse, Response}, routing::post};
    use futures_util::{stream, StreamExt};
    use std::convert::Infallible;
    use tokio::{net::TcpListener, time::timeout};

    async fn model(Json(request): Json<Value>) -> Response {
        let last = request["messages"].as_array().unwrap().last().unwrap();
        if last["content"] == "slow" {
            let chunk = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Waiting\"},\"finish_reason\":null}]}\n\n";
            let body = stream::once(async move { Ok::<_, Infallible>(Bytes::from(chunk)) }).chain(stream::pending());
            return ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(body)).into_response();
        }
        ([(header::CONTENT_TYPE, "text/event-stream")], "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n").into_response()
    }

    #[tokio::test]
    async fn completed_run_publishes_after_restart_with_changed_provider() {
        use hermes_engine::cron::{Draft, RunStatus};
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, Router::new().route("/v1/chat/completions", post(model))).await.unwrap(); });
        let dir = tempfile::tempdir().unwrap();
        let view = Store::open_view(dir.path()).unwrap();
        let job = view.create(Draft { name: "Recovery".into(), prompt: "scheduled".into(), paused: true, ..Draft::default() }, chrono::Utc::now()).unwrap();
        let config = Config { base_url: base, model: "fixture".into(), api_key: String::new() };
        let gateway = Gateway::start(GatewayConfig::new(dir.path(), config)).await.unwrap();
        let routine = routine_for(&gateway.shared, &job.id, job.title()).await.unwrap();
        let routine_id = lock(&routine.summary).id.clone();
        gateway.shared.submit_background(routine.clone(), "slow".into(), "block-routine".into(), false).await.unwrap();
        let claim = gateway.shared.scheduler.claim_gateway_now(&job.id, "manual-once", chrono::Utc::now()).unwrap();
        gateway.shared.scheduler_notify.notify_one();
        timeout(Duration::from_secs(20), async { loop {
            let run = view.runs(&job.id).unwrap().into_iter().find(|run| run.id == claim.run.id).unwrap();
            if let Some(id) = run.gateway_session {
                let session = gateway.shared.session(&id).await.unwrap();
                if gateway.shared.submission_receipt(&session, &format!("cron:{}", run.id)).is_some_and(|receipt| receipt["status"] == "terminal") { break; }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }}).await.unwrap();
        assert_eq!(view.runs(&job.id).unwrap()[0].status, RunStatus::Running);
        drop(routine);
        gateway.shutdown().await.unwrap();
        let invalid = Config { base_url: String::new(), model: "fixture".into(), api_key: String::new() };
        let gateway = Gateway::start(GatewayConfig::new(dir.path(), invalid)).await.unwrap();
        timeout(Duration::from_secs(20), async { loop {
            if view.runs(&job.id).unwrap().iter().any(|run| run.status == RunStatus::Completed) { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }}).await.unwrap();
        let routine = gateway.shared.session(&routine_id).await.unwrap();
        let history = lock(&routine.view).history.clone();
        assert_eq!(history.turns.iter().filter(|turn| turn.prompt == "scheduled").count(), 1);
        assert_eq!(view.runs(&job.id).unwrap().len(), 1);
        drop(routine);
        gateway.shutdown().await.unwrap();
        server.abort();
    }
}
