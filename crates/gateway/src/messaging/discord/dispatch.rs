use super::{
    super::{
        Advance, ConnectionFailure, Disposition, GatewayError, InboundBody, InboundEvent, Message,
        RoomKind,
        runtime::{blocking, db},
        store::Candidate,
    },
    DiscordAdapter, Ingress,
    channels::{ChannelRoute, Member, User, read_id, snowflake},
    socket::{Dispatch, intake_failure},
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
pub(super) struct MessageCreate {
    pub id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub author: User,
    pub member: Option<Member>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(rename = "type", default)]
    pub kind: u8,
    pub webhook_id: Option<String>,
}

impl DiscordAdapter {
    pub(super) async fn dispatch(
        &self,
        ingress: &Ingress,
        frame: Dispatch,
        advance: Advance,
        outgoing: &mpsc::Sender<Value>,
        cancel: &CancellationToken,
    ) -> Result<(), ConnectionFailure> {
        self.observe_channels(&frame.kind, &frame.data);
        self.observe_voice(&frame.kind, &frame.data, ingress);
        match frame.kind.as_str() {
            "MESSAGE_CREATE" => return self.message(ingress, frame.data, advance, cancel).await,
            "INTERACTION_CREATE" => {
                return self
                    .interaction(ingress, frame, advance, outgoing, cancel)
                    .await;
            }
            "GUILD_MEMBER_UPDATE" | "GUILD_MEMBER_REMOVE" => {
                if let (Some(guild), Some(user)) = (
                    read_id(&frame.data, "guild_id"),
                    frame.data.get("user").and_then(|user| read_id(user, "id")),
                ) {
                    self.invalidate_member(ingress, guild, user)
                        .await
                        .map_err(intake_failure)?;
                }
            }
            _ => {}
        }
        skip(ingress, advance).await
    }

    pub(super) async fn message(
        &self,
        ingress: &Ingress,
        data: Value,
        advance: Advance,
        cancel: &CancellationToken,
    ) -> Result<(), ConnectionFailure> {
        let Ok(message) = serde_json::from_value::<MessageCreate>(data.clone()) else {
            return skip(ingress, advance).await;
        };
        let Some(route) = self.route_for_message(&message).await? else {
            return skip(ingress, advance).await;
        };
        if route.room.thread.as_deref() == Some(message.id.as_str())
            && self
                .is_auto_thread_echo(ingress, &route.room.chat, &message.id)
                .map_err(intake_failure)?
        {
            return skip(ingress, advance).await;
        }
        if message.author.id == self.bot.bot_id {
            super::delivery::observe_create(ingress, &self.bot.bot_id, &route.room, &data)
                .await
                .map_err(intake_failure)?;
            return skip(ingress, advance).await;
        }
        if message.webhook_id.is_some() || message.author.system || !matches!(message.kind, 0 | 19)
        {
            return skip(ingress, advance).await;
        }
        let Some(roles) = self
            .roles(&route, &message.author.id, message.member)
            .await?
        else {
            return skip(ingress, advance).await;
        };
        let sender = self
            .record_sender(ingress, &route, &message.author, &roles)
            .await
            .map_err(intake_failure)?;
        let mention = format!("<@{}>", self.bot.bot_id);
        let nick_mention = format!("<@!{}>", self.bot.bot_id);
        let addressed = message
            .mentions
            .iter()
            .any(|user| user.id == self.bot.bot_id)
            && (message.content.contains(&mention) || message.content.contains(&nick_mention));
        let group = route.room.kind == RoomKind::Group;
        let requires_mention = group
            && self.settings.mention_required(
                route.guild.as_deref(),
                &route.room.chat,
                route.room.thread.as_deref(),
            );
        let wrong_mention = group
            && self.settings.suppress_other_mentions
            && !addressed
            && message
                .mentions
                .iter()
                .any(|user| user.id != self.bot.bot_id);
        let thread_continuation = !addressed
            && !wrong_mention
            && self.settings.participated_thread_exemption
            && !route
                .guild
                .as_deref()
                .zip(route.room.thread.as_deref())
                .is_some_and(|(guild, thread)| {
                    self.settings
                        .has_thread_mention_override(guild, &route.room.chat, thread)
                })
            && self
                .participated(ingress, route.room.thread.as_deref())
                .map_err(intake_failure)?;
        let event = InboundEvent {
            id: format!("discord-message-{}", message.id),
            room: route.room,
            sender,
            reply_anchor: Some(message.id.clone()),
            mentioned: !requires_mention || addressed || thread_continuation,
            body: InboundBody::Message(Message::text(
                message
                    .content
                    .replace(&mention, "")
                    .replace(&nick_mention, "")
                    .trim(),
            )),
        };
        let trusted = !message.author.bot
            || addressed && self.settings.trusted_bots.contains(&message.author.id);
        if !trusted || wrong_mention || !event.mentioned {
            refuse(ingress, event, Disposition::Forbidden, advance).await?;
            return Ok(());
        }
        self.accept_message(
            ingress,
            event,
            &message.channel_id,
            &message.id,
            data,
            advance,
            cancel,
        )
        .await
    }

    pub(super) async fn route_for_message(
        &self,
        message: &MessageCreate,
    ) -> Result<Option<ChannelRoute>, ConnectionFailure> {
        if !snowflake(&message.id)
            || !snowflake(&message.author.id)
            || message
                .guild_id
                .as_deref()
                .is_some_and(|guild| !snowflake(guild))
        {
            return Ok(None);
        }
        Ok(self
            .channel_route(&message.channel_id, message.guild_id.as_deref())
            .await?
            .filter(|route| route.guild == message.guild_id))
    }

    fn participated(&self, ingress: &Ingress, thread: Option<&str>) -> Result<bool, GatewayError> {
        let Some(thread) = thread else {
            return Ok(false);
        };
        db(ingress.store.read(|db| {
            Ok(db.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM messaging_deliveries d JOIN messaging_parts p ON p.delivery=d.id
                   WHERE d.connection=?1 AND json_extract(d.intent,'$.room.thread')=?2
                   AND p.stage='sent' AND p.editable_message IS NOT NULL
                 )",
                rusqlite::params![ingress.connection, thread],
                |row| row.get(0),
            )?)
        }))
    }
}

pub(super) async fn skip(ingress: &Ingress, advance: Advance) -> Result<(), ConnectionFailure> {
    ingress
        .submit(Vec::new(), Some(advance))
        .await
        .map_err(intake_failure)?;
    Ok(())
}

/// Commit refusals that depend on Discord-only authorization alongside the
/// same receive position used for allowed events.
pub(super) async fn refuse(
    ingress: &Ingress,
    event: InboundEvent,
    reason: Disposition,
    advance: Advance,
) -> Result<Disposition, ConnectionFailure> {
    let scope = ingress.clone();
    blocking(move || {
        scope.checked(true, |state| {
            let advance = scope.scoped_advance(Some(advance))?;
            let receipt = db(scope.store.intake(
                &scope.connection,
                state.revision,
                vec![Candidate {
                    event,
                    decision: Err(reason),
                    protection: None,
                }],
                advance.as_ref(),
            ))?
            .ok_or(GatewayError::CheckpointChanged)?;
            receipt
                .receipts
                .into_iter()
                .next()
                .map(|receipt| receipt.disposition)
                .ok_or(GatewayError::TemporarilyUnavailable)
        })
    })
    .await
    .map_err(intake_failure)
}
