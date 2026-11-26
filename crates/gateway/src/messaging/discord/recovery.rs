use super::{
    super::{
        Advance, ConnectionFailure, GatewayError,
        runtime::{db, now},
    },
    DiscordAdapter, Ingress,
    channels::{read_id, snowflake_timestamp},
    client::{RestFailure, json_response},
    socket::intake_failure,
};
use reqwest::Method;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

pub(in crate::messaging) const RECOVERY_TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_discord_recovery (
    connection TEXT PRIMARY KEY, cutoff_ms INTEGER NOT NULL
);
";

impl DiscordAdapter {
    pub(super) fn remember_recovery(&self, ingress: &Ingress) -> Result<(), GatewayError> {
        if self.settings.recovery.history_window_secs == 0 {
            return Ok(());
        }
        let window = i64::try_from(self.settings.recovery.history_window_secs)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000);
        ingress.checked(true, |_| db(ingress.store.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO messaging_discord_recovery(connection,cutoff_ms) VALUES (?1,?2)",
                params![ingress.connection, now().saturating_sub(window)],
            )?;
            Ok(())
        })))
    }

    pub(super) async fn recover_history(
        &self,
        ingress: &Ingress,
        cancel: &CancellationToken,
    ) -> Result<(), ConnectionFailure> {
        if self.settings.recovery.history_window_secs == 0 {
            return Ok(());
        }
        let cutoff: Option<i64> = db(ingress.store.read(|tx| {
            Ok(tx
                .query_row(
                    "SELECT cutoff_ms FROM messaging_discord_recovery WHERE connection=?1",
                    [&ingress.connection],
                    |row| row.get(0),
                )
                .optional()?)
        }))
        .map_err(intake_failure)?;
        let Some(cutoff) = cutoff else { return Ok(()) };
        let mut surfaces = BTreeMap::new();
        for (guild, settings) in &self.settings.guilds {
            for (channel, rules) in &settings.channels {
                surfaces.insert(channel.as_str(), guild.as_str());
                for thread in rules.threads.keys() {
                    surfaces.insert(thread.as_str(), guild.as_str());
                }
            }
        }
        let max_surfaces = self.settings.recovery.max_surfaces;
        let mut remaining = self.settings.recovery.max_messages;
        for (channel, guild) in surfaces.into_iter().take(max_surfaces) {
            if cancel.is_cancelled() {
                return Err(ConnectionFailure::Stopped);
            }
            if remaining == 0 {
                break;
            }
            if self.channel_route(channel, Some(guild)).await?.is_none() {
                continue;
            }
            let stream = format!("discord.history.{channel}");
            let mut cursor = ingress
                .checkpoint(stream.clone())
                .await
                .map_err(intake_failure)?;
            let floor = cursor
                .as_deref()
                .and_then(snowflake_timestamp)
                .unwrap_or(cutoff)
                .max(cutoff);
            let mut page = Vec::new();
            let mut before = None;
            let mut covered = false;
            while remaining != 0 && !cancel.is_cancelled() {
                let page_size = remaining.min(100);
                let mut request = self
                    .client
                    .bot(Method::GET, &["channels", channel, "messages"])
                    .query(&[("limit", page_size)]);
                if let Some(before) = before.as_ref() {
                    request = request.query(&[("before", before)]);
                }
                let list: Result<Vec<Value>, RestFailure> = match request.send().await {
                    Ok(response) => json_response(response).await,
                    Err(_) => Err(RestFailure::Unavailable),
                };
                let list = match list {
                    Ok(list) if list.len() <= page_size => list,
                    Ok(_)
                    | Err(RestFailure::Invalid | RestFailure::Missing | RestFailure::Forbidden) => {
                        break;
                    }
                    Err(error) => return Err(super::channels::rest_failure(error)),
                };
                remaining -= list.len();
                let short = list.len() < page_size;
                let oldest = list
                    .iter()
                    .filter_map(|message| read_id(message, "id"))
                    .min_by_key(|id| id.parse::<u64>().unwrap_or_default());
                if oldest.is_some_and(|id| before.as_deref() == Some(id)) {
                    break;
                }
                let reached_floor = oldest
                    .and_then(snowflake_timestamp)
                    .is_some_and(|date| date < floor);
                before = oldest.map(str::to_owned);
                page.extend(list);
                if short || reached_floor {
                    covered = true;
                    break;
                }
                if before.is_none() {
                    break;
                }
            }
            if !covered {
                // The uncovered interval remains visible. Leave the cursor at
                // its prior proven boundary so a later scan can try again.
                ingress.report_gap().map_err(intake_failure)?;
                continue;
            }
            page.sort_by_key(|message| {
                read_id(message, "id").and_then(|id| id.parse::<u64>().ok())
            });
            for mut raw in page {
                let Some(id) = read_id(&raw, "id").map(str::to_owned) else {
                    continue;
                };
                let invalid = read_id(&raw, "channel_id") != Some(channel)
                    || snowflake_timestamp(&id).is_none_or(|date| date < floor)
                    || cursor
                        .as_deref()
                        .and_then(|id| id.parse::<u64>().ok())
                        .is_some_and(|n| id.parse::<u64>().is_ok_and(|id| id <= n));
                if invalid {
                    continue;
                }
                raw["guild_id"] = json!(guild);
                let advance = Advance {
                    stream: stream.clone(),
                    previous: cursor.clone(),
                    checkpoint: id.clone(),
                };
                self.message(ingress, raw, advance, cancel).await?;
                cursor = Some(id);
            }
        }
        if cancel.is_cancelled() {
            return Err(ConnectionFailure::Stopped);
        }
        ingress
            .checked(true, |_| {
                db(ingress.store.write(|tx| {
                    tx.execute(
                        "DELETE FROM messaging_discord_recovery WHERE connection=?1",
                        [&ingress.connection],
                    )?;
                    Ok(())
                }))
            })
            .map_err(intake_failure)
    }
}
