//! Bundled Discord transport for the profile-owned messaging gateway.

mod channels;
mod client;
mod delivery;
mod dispatch;
mod interactions;
mod media;
mod messages;
mod recovery;
pub mod settings;
mod socket;
mod voice;

#[cfg(test)]
mod fixtures;

pub use settings::{DiscordConnectionSettings, DiscordSettings};

use super::{
    Adapter, AdapterFuture, ConnectionFailure, DeliveryBody, DeliveryIntent, DeliveryPart,
    DeliveryResult, Effect, Ingress, Message, PreparationFailure, Room, RoomKind, SendAttempt,
    Sender, VerifiedInstallation,
};
use channels::Channel;
use client::DiscordClient;
use delivery::{DeliverySurface, PlanOptions};
use settings::VerifiedBot;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use voice::Voice;

pub(super) use messages::MESSAGE_TABLES;
pub(super) use recovery::RECOVERY_TABLES;
pub(super) use settings::DiscordFactory;

pub(super) struct DiscordAdapter {
    client: DiscordClient,
    settings: DiscordSettings,
    bot: VerifiedBot,
    channels: Mutex<BTreeMap<String, Channel>>,
    callbacks: Arc<Semaphore>,
    preparations: Arc<Mutex<BTreeSet<String>>>,
    voice: Voice,
}

impl DiscordAdapter {
    fn new(settings: DiscordSettings, client: DiscordClient, bot: VerifiedBot) -> Self {
        let voice = Voice::new(bot.bot_id.parse().unwrap_or_default());
        Self {
            client,
            settings,
            bot,
            channels: Mutex::default(),
            callbacks: Arc::new(Semaphore::new(32)),
            preparations: Arc::default(),
            voice,
        }
    }

    async fn transport_ready(
        &self,
        ingress: &Ingress,
        fresh: bool,
        cancel: &CancellationToken,
    ) -> Result<(), ConnectionFailure> {
        if let Some(destination) = &self.settings.default_delivery_target {
            let id = destination
                .thread
                .as_deref()
                .unwrap_or(&destination.channel);
            let route = self.channel_route(id, Some(&destination.guild)).await?;
            if let Some(route) = route {
                ingress
                    .verify_destination(route.room, super::runtime::now() + 86_400_000)
                    .await
                    .map_err(socket::intake_failure)?;
            }
        }
        if fresh {
            self.remember_recovery(ingress)
                .map_err(socket::intake_failure)?;
        }
        self.recover_history(ingress, cancel).await?;
        self.resume_preparations(ingress, cancel).await?;
        if self.settings.commands.enabled && self.settings.commands.sync {
            let client = self.client.clone();
            let application = self.bot.application_id.clone();
            let guild = self.settings.commands.test_guild.clone();
            let voice = self.settings.voice_enabled;
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => {},
                    _ = interactions::synchronize(&client, &application, guild.as_deref(), voice) => {}
                }
            });
        }
        Ok(())
    }

    fn observe_voice(&self, kind: &str, payload: &serde_json::Value, ingress: &Ingress) {
        if self.settings.voice_enabled {
            self.voice.observe(kind, payload, ingress);
        }
    }

    async fn join_voice(
        &self,
        ingress: &Ingress,
        actor: &Sender,
        room: &Room,
        guild: &str,
    ) -> Result<serde_json::Value, super::GatewayError> {
        self.voice.join(ingress, actor, room, guild)
    }

    fn voice_housekeeping(&self, ingress: &Ingress) -> Vec<serde_json::Value> {
        if self.settings.voice_enabled {
            self.voice.housekeeping(ingress)
        } else {
            Vec::new()
        }
    }

    fn leave_voice(
        &self,
        ingress: &Ingress,
        actor: &Sender,
        room: &Room,
        guild: &str,
    ) -> Option<serde_json::Value> {
        self.voice
            .leave_bound(ingress, actor, room, guild)
            .ok()
            .flatten()
    }
}

struct ResetVoice<'a>(&'a Voice);

impl Drop for ResetVoice<'_> {
    fn drop(&mut self) {
        self.0.reset();
    }
}

impl Adapter for DiscordAdapter {
    fn initial_transport_ready(&self) -> bool {
        false
    }

    fn verified_installation(&self) -> Option<VerifiedInstallation> {
        Some(self.bot.installation())
    }

    fn listen(
        &self,
        ingress: Ingress,
        cancel: CancellationToken,
    ) -> AdapterFuture<'_, ConnectionFailure> {
        Box::pin(async move {
            let _voice = ResetVoice(&self.voice);
            socket::listen(self, ingress, cancel).await
        })
    }

    fn prepare(&self, intent: &DeliveryIntent) -> Result<Vec<DeliveryPart>, PreparationFailure> {
        let forum = intent.room.kind == RoomKind::Group
            && intent.room.thread.is_none()
            && crate::lock(&self.channels)
                .get(&intent.room.chat)
                .is_some_and(|channel| matches!(channel.kind, 15 | 16));
        let surface = if forum {
            if !self.settings.delivery.forum_posts {
                return Err(PreparationFailure::Unsupported);
            }
            DeliverySurface::ForumPost
        } else {
            DeliverySurface::Existing
        };
        let mut parts = delivery::prepare_with(
            intent,
            PlanOptions {
                reply_mode: self.settings.delivery.reply_mode,
                uploads: self.settings.delivery.uploads,
                max_inline_parts: self.settings.delivery.max_inline_parts,
                surface,
            },
        )?;
        if self.settings.voice_enabled
            && let Some(operation_key) = self.voice.playback_key(intent)
            && let DeliveryBody::Reply(message) = &intent.body
            && !message.text.trim().is_empty()
        {
            parts.push(DeliveryPart {
                room: intent.room.clone(),
                body: DeliveryBody::Reply(Message::text(&message.text)),
                edit_target: None,
                effect: Effect {
                    operation_key: Some(operation_key),
                    ..Effect::default()
                },
            });
        }
        Ok(parts)
    }

    fn send(
        &self,
        attempt: SendAttempt,
        ingress: Ingress,
        cancel: CancellationToken,
    ) -> AdapterFuture<'_, DeliveryResult> {
        Box::pin(async move {
            if Voice::playback(&attempt) {
                return self.voice.play(&ingress, &attempt).await;
            }
            delivery::send(&self.client, &self.bot.bot_id, attempt, ingress, cancel).await
        })
    }
}
