#[cfg(test)]
mod tests;

use super::super::{LiveSession, Settings, Shared, TELEGRAM_ROUTE_PREFIX, TELEGRAM_SOURCE, lock};
use hermes_telegram::{
    Client, DeliveryOutcome, ErrorKind, Identity, Store, Token, TurnOutcome, Work, WorkStage,
};
use std::{
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    task::JoinSet,
    time::{self, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

pub(super) struct Connector {
    pub(super) store: Store,
    pub(super) client: Client,
    pub(super) bot: Identity,
    pub(super) cancel: CancellationToken,
}

impl Connector {
    pub(super) async fn run(self: &Arc<Self>, owner: Option<Weak<Shared>>) -> Result<(), String> {
        let (poll, agent) = tokio::join!(
            async {
                let result = self.poll().await;
                self.cancel.cancel();
                result
            },
            async {
                let result = if let Some(owner) = owner {
                    self.agent(owner).await
                } else {
                    self.cancel.cancelled().await;
                    Ok(())
                };
                self.cancel.cancel();
                result
            },
        );
        poll.and(agent)
    }

    async fn stored<T: Send + 'static>(
        &self,
        action: impl FnOnce(&Store, &Identity, &Token) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let store = self.store.clone();
        let bot = self.bot.clone();
        let token = self.client.credential().clone();
        tokio::task::spawn_blocking(move || action(&store, &bot, &token))
            .await
            .map_err(|_| "Telegram storage task stopped unexpectedly".to_owned())?
    }

    async fn poll(&self) -> Result<(), String> {
        while !self.cancel.is_cancelled() {
            let offset = self
                .stored(|store, bot, _| store.poll_offset(bot, now()))
                .await?;
            match self.client.poll(self.bot.id, offset, &self.cancel).await {
                Ok(batch) => {
                    self.stored(move |store, bot, token| {
                        store.commit_poll(
                            bot,
                            token,
                            &batch.messages,
                            offset,
                            batch.next_offset,
                            now(),
                        )
                    })
                    .await?;
                    if batch.next_offset == offset {
                        // A server returning immediately must not spin the gateway.
                        self.wait(Duration::from_millis(100)).await;
                    }
                }
                Err(error)
                    if matches!(error.kind, ErrorKind::Retryable | ErrorKind::RateLimited) =>
                {
                    self.wait(Duration::from_secs(error.retry_after.unwrap_or(3).max(1)))
                        .await;
                }
                Err(error) if error.kind == ErrorKind::Cancelled => return Ok(()),
                Err(error) => return Err(format!("Telegram polling stopped ({:?})", error.kind)),
            }
        }
        Ok(())
    }

    async fn wait(&self, duration: Duration) {
        tokio::select! {
            _ = self.cancel.cancelled() => {}
            _ = time::sleep(duration) => {}
        }
    }

    async fn agent(self: &Arc<Self>, owner: Weak<Shared>) -> Result<(), String> {
        let mut tick = time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut sends = JoinSet::new();
        let mut result = loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break Ok(()),
                Some(sent) = sends.join_next(), if !sends.is_empty() => {
                    if let Err(error) = joined_send(sent) { break Err(error); }
                }
                _ = tick.tick() => {
                    let Some(owner) = owner.upgrade() else { break Ok(()); };
                    if let Err(error) = self.advance(owner, &mut sends).await { break Err(error); }
                }
            }
        };
        self.cancel.cancel();
        // A cancelled request may already have reached Telegram. Keep the
        // bot lease until all claimed attempts have recorded their outcome.
        while let Some(sent) = sends.join_next().await {
            result = result.and(joined_send(sent));
        }
        result
    }

    async fn advance(
        self: &Arc<Self>,
        owner: Arc<Shared>,
        sends: &mut JoinSet<Result<(), String>>,
    ) -> Result<(), String> {
        let heads = self.stored(|store, bot, _| store.ready(bot, now())).await?;
        for work in heads {
            if self.cancel.is_cancelled() {
                break;
            }
            match work.stage {
                WorkStage::Received | WorkStage::TurnUnknown => {
                    self.advance_turn(&owner, work).await?
                }
                WorkStage::Pending => {
                    let session = match self.session_for(&owner, work.chat_id, false).await? {
                        Route::Session(session) => session,
                        Route::Busy => continue,
                        Route::Missing | Route::Blocked => {
                            self.record_turn(
                                work.update_id,
                                TurnOutcome::Blocked("route_unavailable"),
                            )
                            .await?;
                            continue;
                        }
                    };
                    let (work, attempt) = self
                        .stored(move |store, bot, token| {
                            let attempt = store.begin_send(bot, token, &work, now())?;
                            Ok((work, attempt))
                        })
                        .await?;
                    if let Some(attempt) = attempt {
                        let connector = self.clone();
                        sends.spawn(async move {
                            // A catalog delete must wait for an already claimed delivery.
                            let _session = session;
                            connector.send(work, attempt).await
                        });
                    }
                }
            }
        }
        Ok(())
    }

    async fn advance_turn(&self, owner: &Arc<Shared>, work: Work) -> Result<(), String> {
        let chat = work.chat_id;
        let allowed = self
            .stored(move |store, bot, token| {
                store
                    .with_agent_policy(bot, token, chat, || Ok(()))
                    .map(|checked| checked.is_some())
            })
            .await?;
        let session = match self
            .session_for(owner, chat, allowed && work.stage == WorkStage::Received)
            .await?
        {
            Route::Session(session) => session,
            Route::Busy => return Ok(()),
            Route::Missing if !allowed && work.stage == WorkStage::Received => return Ok(()),
            Route::Missing | Route::Blocked => {
                return self
                    .record_turn(work.update_id, TurnOutcome::Blocked("route_unavailable"))
                    .await;
            }
        };
        let input = submission_key(self.bot.id, work.update_id);
        let receipt = match owner.submission_receipt(&session, &input) {
            Some(receipt) => receipt,
            None if work.stage == WorkStage::TurnUnknown => {
                return self
                    .record_turn(work.update_id, TurnOutcome::Blocked("receipt_unavailable"))
                    .await;
            }
            None if !allowed => return Ok(()),
            None => {
                match owner
                    .submit_telegram(session.clone(), work.text, input)
                    .await
                {
                    Ok(receipt) => receipt,
                    Err(error) if error.data["reason"] == "shutdown" => return Ok(()),
                    Err(error) if error.data["reason"] == "admission_conflict" => {
                        return self
                            .record_turn(work.update_id, TurnOutcome::Blocked("admission_conflict"))
                            .await;
                    }
                    Err(_) => {
                        return self
                            .record_turn(work.update_id, TurnOutcome::Failed("admission_failed"))
                            .await;
                    }
                }
            }
        };
        match receipt["status"].as_str() {
            Some("queued") if allowed && !self.cancel.is_cancelled() => {
                let owner_for_claim = owner.clone();
                let session_for_claim = session.clone();
                let claim = self
                    .stored(move |store, bot, token| {
                        store
                            .with_agent_policy(bot, token, chat, || {
                                owner_for_claim
                                    .claim_telegram_turn(&session_for_claim)
                                    .map_err(|e| e.to_string())
                            })
                            .map(Option::flatten)
                    })
                    .await?;
                if let Some(claim) = claim {
                    owner.spawn_worker(session, claim).await;
                }
                Ok(())
            }
            Some("unknown") if work.stage != WorkStage::TurnUnknown => {
                self.record_turn(work.update_id, TurnOutcome::Unknown).await
            }
            Some("terminal") if receipt["outcome"] == "complete" => {
                let outcome = receipt["result"]["text"]
                    .as_str()
                    .and_then(reply_text)
                    .map(TurnOutcome::Reply)
                    .unwrap_or(TurnOutcome::Failed("empty_reply"));
                self.record_turn(work.update_id, outcome).await
            }
            Some("terminal") => {
                self.record_turn(work.update_id, TurnOutcome::Failed("turn_incomplete"))
                    .await
            }
            _ => Ok(()),
        }
    }

    async fn session_for(
        &self,
        owner: &Arc<Shared>,
        chat: i64,
        create: bool,
    ) -> Result<Route, String> {
        let key = format!("{TELEGRAM_ROUTE_PREFIX}{}:{chat}", self.bot.id);
        let id = match self
            .stored(move |store, bot, _| store.route(bot, chat))
            .await?
        {
            Some(id) => id,
            None if !create => return Ok(Route::Missing),
            None => {
                let owner = owner.clone();
                let creation_key = key.clone();
                let created = tokio::task::spawn_blocking(move || {
                    let mut conflict = false;
                    let result = owner.library.create_background_session(
                        &format!("Telegram · {chat}"),
                        &creation_key,
                        |summary| {
                            let settings = Settings::read(&summary.directory)?;
                            if settings.is_telegram_session()
                                && settings.creation_key.as_ref() == Some(&creation_key)
                            {
                                return Ok(());
                            }
                            if !settings.source.is_empty()
                                || settings.creation_key.is_some()
                                || hermes_engine::has_conversation(&summary.directory)?
                            {
                                conflict = true;
                                anyhow::bail!("This conversation is owned by a different route");
                            }
                            Settings {
                                source: TELEGRAM_SOURCE.to_owned(),
                                creation_key: Some(creation_key.clone()),
                                ..Default::default()
                            }
                            .save(&summary.directory)
                        },
                    );
                    if conflict {
                        Ok(None)
                    } else {
                        result
                            .map(|summary| Some(summary.id))
                            .map_err(|error| error.to_string())
                    }
                })
                .await
                .map_err(|_| "Telegram route creation stopped unexpectedly".to_owned())??;
                let Some(created) = created else {
                    return Ok(Route::Blocked);
                };
                let mapped = created.clone();
                self.stored(move |store, bot, _| store.save_route(bot, chat, &mapped))
                    .await?;
                created
            }
        };
        match owner.session(&id).await {
            Ok(session) => {
                let matches = {
                    let settings = lock(&session.settings);
                    settings.is_telegram_session() && settings.creation_key.as_deref() == Some(&key)
                };
                Ok(if matches {
                    Route::Session(session)
                } else {
                    Route::Blocked
                })
            }
            Err(error)
                if matches!(
                    error.data["reason"].as_str(),
                    Some("session_limit" | "session_busy")
                ) =>
            {
                Ok(Route::Busy)
            }
            Err(_) => Ok(Route::Blocked),
        }
    }

    async fn record_turn(&self, id: i64, outcome: TurnOutcome) -> Result<(), String> {
        self.stored(move |store, bot, _| store.record_turn(bot, id, outcome))
            .await
    }

    async fn send(&self, work: Work, attempt: i64) -> Result<(), String> {
        let outcome = match self
            .client
            .send(work.chat_id, &work.text, &self.cancel)
            .await
        {
            Ok(receipt) => DeliveryOutcome::Sent(receipt),
            Err(error) if error.kind == ErrorKind::RateLimited => {
                let seconds = error.retry_after.unwrap_or(3).max(1);
                DeliveryOutcome::RateLimited {
                    retry_at: now().saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX)),
                }
            }
            Err(error) if error.kind == ErrorKind::Rejected => DeliveryOutcome::Failed,
            Err(_) => DeliveryOutcome::Unknown,
        };
        self.stored(move |store, bot, _| store.record_delivery(bot, &work, attempt, outcome))
            .await
    }
}

enum Route {
    Session(Arc<LiveSession>),
    Busy,
    Missing,
    Blocked,
}

pub(crate) fn submission_key(bot: i64, update: i64) -> String {
    format!("telegram:{bot}:{update}")
}

fn reply_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    const MAX_REPLY_CHARS: usize = 4096;
    const SUFFIX: &str = "\n… [truncated]";
    if text.chars().count() <= MAX_REPLY_CHARS {
        return Some(text.to_owned());
    }
    let mut reply: String = text
        .chars()
        .take(MAX_REPLY_CHARS - SUFFIX.chars().count())
        .collect();
    reply.push_str(SUFFIX);
    Some(reply)
}

fn joined_send(result: Result<Result<(), String>, tokio::task::JoinError>) -> Result<(), String> {
    result.map_err(|_| "Telegram delivery task stopped unexpectedly".to_owned())?
}

pub(super) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
