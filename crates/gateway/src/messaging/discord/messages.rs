use super::{
    super::{
        Advance, ConnectionFailure, DeferredInput, DeliveryResult, Disposition, EffectReceipt,
        GatewayError, Handoff, HandoffItem, InboundBody, InboundEvent, MediaSource, Message,
        Preparation, PreparationPurpose, QuoteHydration, QuoteLookup, QuoteReference, ReceiptKind,
        RoomKind,
        prepared_threads::ThreadPreparation,
        runtime::{blocking, db, now},
        store,
    },
    DiscordAdapter, Ingress,
    channels::{read_id, snowflake},
    client::{DiscordClient, RestFailure},
    dispatch::refuse,
    media::{self, AttachmentSource},
    socket::intake_failure,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

const PREPARATION_TTL_MS: i64 = 10 * 60 * 1000;
const MAX_WORKERS: usize = 12;
const TOKEN_PREFIX: &str = "discord-preparation-";

pub(in crate::messaging) const MESSAGE_TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_discord_preparation (
    connection TEXT NOT NULL, event TEXT NOT NULL, expires INTEGER NOT NULL, manifest TEXT NOT NULL,
    PRIMARY KEY(connection,event)
);
";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Manifest {
    channel: String,
    message: String,
    attachments: Vec<AttachmentSource>,
    quote: Option<QuoteReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread: Option<AutoThread>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct AutoThread {
    guild: String,
    inline_on_refusal: bool,
}

impl DiscordAdapter {
    pub(super) fn is_auto_thread_echo(
        &self,
        ingress: &Ingress,
        parent: &str,
        message: &str,
    ) -> Result<bool, GatewayError> {
        db(ingress.store.read(|db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM messaging_discord_preparation WHERE connection=?1
                 AND event=?2 AND json_extract(manifest,'$.channel')=?3
                 AND json_type(manifest,'$.thread')='object')",
                params![
                    ingress.connection,
                    format!("discord-message-{message}"),
                    parent
                ],
                |row| row.get(0),
            )?)
        }))
    }
    pub(super) async fn accept_message(
        &self,
        ingress: &Ingress,
        event: InboundEvent,
        channel: &str,
        message: &str,
        raw: Value,
        advance: Advance,
        cancel: &CancellationToken,
    ) -> Result<(), ConnectionFailure> {
        let attachments = match media::attachment_sources(&raw) {
            Ok(attachments) => attachments,
            Err(_) => {
                refuse(ingress, event, Disposition::Invalid, advance).await?;
                return Ok(());
            }
        };
        if !attachments.is_empty()
            && (!self.settings.media.attachments
                || !media::within_inbound_limit(
                    &attachments,
                    self.settings.media.max_inbound_bytes,
                ))
        {
            refuse(ingress, event, Disposition::Unsupported, advance).await?;
            return Ok(());
        }
        let quote = raw
            .get("message_reference")
            .filter(|reference| {
                reference
                    .get("channel_id")
                    .and_then(Value::as_str)
                    .is_none_or(|id| id == channel)
            })
            .and_then(|reference| read_id(reference, "message_id"))
            .filter(|source| *source != message)
            .map(|message_id| QuoteReference {
                room: event.room.clone(),
                message_id: message_id.to_owned(),
            });
        let thread = if self.settings.auto_thread
            && event.room.kind == RoomKind::Group
            && event.room.thread.is_none()
            && quote.is_none()
            && raw.get("type").and_then(Value::as_u64).unwrap_or(0) == 0
            && crate::lock(&self.channels)
                .get(channel)
                .is_some_and(|channel| matches!(channel.kind, 0 | 5))
        {
            read_id(&raw, "guild_id").map(|guild| AutoThread {
                guild: guild.to_owned(),
                inline_on_refusal: self.settings.auto_thread_inline_fallback,
            })
        } else {
            None
        };
        if attachments.is_empty() && quote.is_none() && thread.is_none() {
            ingress
                .submit(vec![event], Some(advance))
                .await
                .map_err(intake_failure)?;
            return Ok(());
        }

        let authorized = ingress
            .checked(true, |state| {
                Ok(ingress
                    .route_candidate(&state.policy, &event, true)?
                    .is_ok())
            })
            .map_err(intake_failure)?;
        if !authorized {
            refuse(ingress, event, Disposition::Forbidden, advance).await?;
            return Ok(());
        }

        let manifest = Manifest {
            channel: channel.to_owned(),
            message: message.to_owned(),
            attachments,
            quote,
            thread,
        };
        let id = event.id.clone();
        let scope = ingress.clone();
        let durable_manifest = manifest.clone();
        let event_key = id.clone();
        let expires_ms = blocking(move || scope.checked(true, |_| {
            db(scope.store.write(|tx| {
                let serialized = serde_json::to_string(&durable_manifest)?;
                let expires = now() + PREPARATION_TTL_MS;
                tx.execute(
                    "INSERT OR IGNORE INTO messaging_discord_preparation(connection,event,expires,manifest)
                       VALUES (?1,?2,?3,?4)",
                    params![scope.connection,event_key,expires,serialized],
                )?;
                let stored: (i64, String) = tx.query_row(
                    "SELECT expires,manifest FROM messaging_discord_preparation WHERE connection=?1 AND event=?2",
                    params![scope.connection,event_key], |row| Ok((row.get(0)?,row.get(1)?)),
                )?;
                Ok((stored.1 == serialized).then_some(stored.0))
            }))
        })).await.map_err(intake_failure)?;
        let Some(expires_ms) = expires_ms else {
            let scope = ingress.clone();
            let key = event.id.clone();
            let cursor = advance.clone();
            blocking(move || scope.checked(true, |_| {
                let cursor = scope.scoped_advance(Some(cursor))?;
                db(scope.store.write(|tx| {
                    if !store::check_advance(tx, &scope.connection, cursor.as_ref())? {
                        return Ok(Err(GatewayError::CheckpointChanged));
                    }
                    tx.execute("UPDATE messaging_ingress SET conflict=1 WHERE connection=?1 AND event=?2",
                        params![scope.connection,key])?;
                    store::write_advance(tx, &scope.connection, cursor.as_ref())?;
                    Ok(Ok(()))
                }))?
            })).await.map_err(intake_failure)?;
            return Ok(());
        };
        let token = db(store::key("discord-preparation", &id)).map_err(intake_failure)?;
        let handoff_id = db(store::key("discord-handoff", &(&id, &advance.checkpoint)))
            .map_err(intake_failure)?;
        let preparation = Preparation {
            token,
            expires_ms,
            purpose: if let Some(thread) = &manifest.thread {
                PreparationPurpose::Thread {
                    id: manifest.message.clone(),
                    inline_on_refusal: thread.inline_on_refusal,
                }
            } else {
                manifest
                    .quote
                    .clone()
                    .map_or(PreparationPurpose::Media, PreparationPurpose::Quote)
            },
            protection: None,
        };
        let receipt = ingress
            .stage_handoff(Handoff {
                id: handoff_id,
                items: vec![HandoffItem {
                    event: event.clone(),
                    preparation: Some(preparation.clone()),
                }],
                advance: Some(advance),
            })
            .await
            .map_err(intake_failure)?;
        if receipt
            .receipts
            .first()
            .is_some_and(|receipt| receipt.disposition == Disposition::Deferred)
        {
            self.start_preparation(
                ingress,
                DeferredInput { event, preparation },
                manifest,
                cancel,
            );
        }
        Ok(())
    }

    pub(super) async fn resume_preparations(
        &self,
        ingress: &Ingress,
        cancel: &CancellationToken,
    ) -> Result<(), ConnectionFailure> {
        let pending = ingress
            .pending_preparations()
            .await
            .map_err(intake_failure)?;
        for input in pending {
            if !input.preparation.token.starts_with(TOKEN_PREFIX) {
                continue;
            }
            let manifest: Option<Manifest> = db(ingress.store.read(|tx| {
                Ok(tx.query_row(
                    "SELECT manifest FROM messaging_discord_preparation WHERE connection=?1 AND event=?2",
                    params![ingress.connection,input.event.id], |row| store::decode(row, 0),
                ).optional()?)
            })).map_err(intake_failure)?;
            if let Some(manifest) = manifest {
                self.start_preparation(ingress, input, manifest, cancel);
            }
        }
        Ok(())
    }

    fn start_preparation(
        &self,
        ingress: &Ingress,
        input: DeferredInput,
        manifest: Manifest,
        cancel: &CancellationToken,
    ) {
        let event = input.event.id.clone();
        let mut active = crate::lock(&self.preparations);
        if active.len() >= MAX_WORKERS || !active.insert(event.clone()) {
            return;
        }
        drop(active);
        let lease = PreparationWorker {
            workers: self.preparations.clone(),
            event,
        };
        let client = self.client.clone();
        let bot = self.bot.bot_id.clone();
        let max_inbound_bytes = self.settings.media.max_inbound_bytes;
        let ingress = ingress.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _worker = lease;
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = hydrate(&client, &bot, &ingress, input, manifest, max_inbound_bytes) => {},
            }
        });
    }
}

struct PreparationWorker {
    workers: Arc<Mutex<BTreeSet<String>>>,
    event: String,
}

impl Drop for PreparationWorker {
    fn drop(&mut self) {
        crate::lock(&self.workers).remove(&self.event);
    }
}

async fn hydrate(
    client: &DiscordClient,
    bot: &str,
    ingress: &Ingress,
    mut input: DeferredInput,
    manifest: Manifest,
    max_inbound_bytes: u64,
) -> Result<(), GatewayError> {
    if let Some(thread) = &manifest.thread {
        match ingress
            .prepare_thread(input.event.id.clone(), input.preparation.token.clone())
            .await?
        {
            ThreadPreparation::Ready(event) => input.event = event,
            ThreadPreparation::Reconcile(attempt) => {
                let thread_id = manifest.message.as_str();
                let channel: Result<Value, _> = client.get(&["channels", thread_id]).await;
                if let Ok(channel) = channel
                    && read_id(&channel, "id") == Some(thread_id)
                    && read_id(&channel, "parent_id") == Some(manifest.channel.as_str())
                    && read_id(&channel, "guild_id") == Some(thread.guild.as_str())
                    && read_id(&channel, "owner_id") == Some(bot)
                    && matches!(channel["type"].as_u64(), Some(10 | 11))
                {
                    ingress
                        .reconcile_delivery(
                            attempt,
                            DeliveryResult::EffectConfirmed(EffectReceipt {
                                kind: ReceiptKind::Destination,
                                id: thread_id.to_owned(),
                            }),
                        )
                        .await?;
                }
                return Ok(());
            }
            ThreadPreparation::Waiting | ThreadPreparation::Closed => return Ok(()),
        }
    }
    let source = MediaSource::from(&input.event);
    let InboundBody::Message(body) = input.event.body else {
        return Err(GatewayError::Invalid);
    };
    let event = &source.event_id;
    let token = &input.preparation.token;
    if let PreparationPurpose::Quote(_) = input.preparation.purpose {
        ingress
            .hydrate_quote(event, token, |requested| async {
                let message = if manifest.attachments.is_empty() {
                    None
                } else {
                    Some(
                        media_body(client, ingress, &source, &manifest, body, max_inbound_bytes)
                            .await?,
                    )
                };
                let quote = get_quote(client, &manifest.channel, requested).await?;
                Ok(QuoteHydration {
                    message,
                    quote,
                    advance: None,
                })
            })
            .await?;
    } else {
        ingress
            .authorize_preparation(event.clone(), token.clone())
            .await?;
        let message =
            match media_body(client, ingress, &source, &manifest, body, max_inbound_bytes).await {
                Ok(message) => message,
                Err(
                    GatewayError::Invalid | GatewayError::Unsupported | GatewayError::Forbidden,
                ) => Message::text(""),
                Err(error) => return Err(error),
            };
        ingress
            .complete_preparation(event.clone(), token.clone(), message)
            .await?;
    }
    Ok(())
}

async fn media_body(
    client: &DiscordClient,
    ingress: &Ingress,
    source: &MediaSource,
    manifest: &Manifest,
    body: Message,
    max_inbound_bytes: u64,
) -> Result<Message, GatewayError> {
    if manifest.attachments.is_empty() {
        return Ok(body);
    }
    media::hydrate_attachments(
        client,
        ingress,
        source,
        &manifest.channel,
        &manifest.message,
        body,
        &manifest.attachments,
        max_inbound_bytes,
    )
    .await
}

async fn get_quote(
    client: &DiscordClient,
    channel: &str,
    requested: QuoteReference,
) -> Result<QuoteLookup, GatewayError> {
    if !snowflake(&requested.message_id) {
        return Ok(QuoteLookup::Unavailable);
    }
    let response: Result<Value, _> = client
        .get(&["channels", channel, "messages", &requested.message_id])
        .await;
    match response {
        Ok(message)
            if read_id(&message, "channel_id") == Some(channel)
                && read_id(&message, "id") == Some(requested.message_id.as_str()) =>
        {
            let text = message.get("content").and_then(Value::as_str).unwrap_or("");
            if text.trim().is_empty() || text.len() > 8 * 1024 {
                return Ok(QuoteLookup::Unavailable);
            }
            Ok(QuoteLookup::Found {
                source: requested,
                text: text.into(),
            })
        }
        Ok(_) | Err(RestFailure::Missing | RestFailure::Forbidden) => Ok(QuoteLookup::Unavailable),
        Err(_) => Err(GatewayError::TemporarilyUnavailable),
    }
}
