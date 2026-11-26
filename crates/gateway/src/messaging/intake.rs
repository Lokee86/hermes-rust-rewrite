use super::{
    runtime::{Ingress, MAX_BATCH, blocking, db, valid_advance, valid_id},
    store::{self, Candidate, Store},
    types::*,
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_handoffs (
    connection TEXT NOT NULL, id TEXT NOT NULL,
    fingerprint TEXT NOT NULL, receipts TEXT NOT NULL,
    PRIMARY KEY(connection, id)
);
CREATE TABLE IF NOT EXISTS messaging_preparation (
    connection TEXT NOT NULL, event TEXT NOT NULL, token TEXT NOT NULL,
    purpose TEXT NOT NULL, expires INTEGER NOT NULL, finished TEXT,
    PRIMARY KEY(connection, event)
);
CREATE INDEX IF NOT EXISTS messaging_preparation_deadline ON messaging_preparation(connection, expires)
    WHERE finished IS NULL;
";

/// A bridge envelope has its own replay identity. The event IDs within it
/// identify logical input independently of the bridge or poll checkpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Handoff {
    pub id: String,
    pub items: Vec<HandoffItem>,
    pub advance: Option<Advance>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HandoffItem {
    pub event: InboundEvent,
    pub preparation: Option<Preparation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Preparation {
    pub token: String,
    pub purpose: PreparationPurpose,
    pub expires_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationPurpose {
    Body,
    Media,
    ProtectedContent,
    Context,
    Burst,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredInput {
    pub event: InboundEvent,
    pub preparation: Preparation,
}

impl Ingress {
    /// Commit all dispositions before an acknowledgement or an SDK-owned
    /// checkpoint can advance. The bridge can replay this call after restart.
    pub async fn stage_handoff(&self, handoff: Handoff) -> Result<IntakeResult, GatewayError> {
        self.stage(handoff, false).await
    }

    /// Merge a compatible text burst at durable admission. Every original ID
    /// retains a disposition even when several inputs form one turn.
    pub async fn stage_text_burst(&self, handoff: Handoff) -> Result<IntakeResult, GatewayError> {
        let first = handoff.items.first().ok_or(GatewayError::Invalid)?;
        if !handoff.items.iter().all(|item| {
            item.preparation.is_none() && item.event.room == first.event.room
                && item.event.sender.id == first.event.sender.id
                && item.event.reply_anchor == first.event.reply_anchor
                && matches!(&item.event.body, InboundBody::Message(message) if message.attachments.is_empty())
        }) {
            return Err(GatewayError::Invalid);
        }
        self.stage(handoff, true).await
    }

    async fn stage(&self, handoff: Handoff, merge: bool) -> Result<IntakeResult, GatewayError> {
        if !valid_id(&handoff.id)
            || handoff.items.is_empty()
            || handoff.items.len() > MAX_BATCH
            || !valid_advance(handoff.advance.as_ref())
            || handoff.items.iter().any(|item| {
                !super::runtime::valid_input_id(&item.event.id)
                    || matches!(item.event.body, InboundBody::Wake)
                    || item.preparation.as_ref().is_some_and(|p| {
                        !valid_id(&p.token)
                            || p.expires_ms <= 0
                            || !matches!(item.event.body, InboundBody::Message(_))
                    })
            })
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        let result = blocking(move || {
            scope.checked(true, |state| {
                let Handoff { id, items, advance } = handoff;
                let mut candidates = Vec::with_capacity(items.len());
                for mut item in items {
                    let decision = scope.route_candidate(
                        &state.policy,
                        &item.event,
                        item.preparation.is_some(),
                    )?;
                    scope.normalize_event(&mut item.event)?;
                    candidates.push((
                        Candidate {
                            event: item.event,
                            decision,
                        },
                        item.preparation,
                    ));
                }
                db(scope.store.handoff(
                    &scope.connection,
                    state.revision,
                    &id,
                    candidates,
                    advance.as_ref(),
                    merge,
                ))?
            })
        })
        .await?;
        self.notify();
        Ok(result)
    }

    pub async fn pending_preparations(&self) -> Result<Vec<DeferredInput>, GatewayError> {
        let scope = self.clone();
        blocking(move || scope.checked(true, |_| db(scope.store.preparations(&scope.connection))))
            .await
    }

    /// Complete a durable staged input. The adapter supplies only hydrated
    /// content, never a new source actor, destination or logical identity.
    pub async fn complete_preparation(
        &self,
        event_id: String,
        token: String,
        message: Message,
    ) -> Result<IntakeReceipt, GatewayError> {
        if !valid_id(&event_id) || !valid_id(&token) {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        let result = blocking(move || {
            scope.checked(true, |state| {
                let content = db(store::key("prepared-content", &message))?;
                let staged = db(scope.store.preparation(&scope.connection, &event_id))?
                    .ok_or(GatewayError::Invalid)?;
                if token != staged.token {
                    return Err(GatewayError::Forbidden);
                }
                if let Some(finished) = staged.finished {
                    return if finished == content {
                        Ok(IntakeReceipt {
                            repeated: true,
                            ..staged.receipt
                        })
                    } else {
                        Err(GatewayError::Conflict)
                    };
                }
                if staged.receipt.disposition == Disposition::Expired {
                    return Ok(IntakeReceipt {
                        repeated: true,
                        ..staged.receipt
                    });
                }
                let mut event = staged.event.ok_or(GatewayError::Invalid)?;
                event.body = InboundBody::Message(message);
                let decision = if staged.expires_ms <= chrono::Utc::now().timestamp_millis() {
                    Err(Disposition::Expired)
                } else {
                    scope.route_event(&state.policy, &event)?
                };
                db(scope.store.finish_preparation(
                    &scope.connection,
                    &event_id,
                    &token,
                    &content,
                    Candidate { event, decision },
                ))
            })
        })
        .await?;
        self.notify();
        Ok(result)
    }
}

struct Staged {
    token: String,
    expires_ms: i64,
    finished: Option<String>,
    receipt: IntakeReceipt,
    event: Option<InboundEvent>,
}

impl Store {
    fn handoff(
        &self,
        connection: &str,
        revision: i64,
        id: &str,
        candidates: Vec<(Candidate, Option<Preparation>)>,
        advance: Option<&Advance>,
        merge: bool,
    ) -> Result<Result<IntakeResult, GatewayError>> {
        let identities = candidates
            .iter()
            .map(|(candidate, preparation)| {
                Ok((
                    &candidate.event.id,
                    store::fingerprint(&candidate.event)?,
                    preparation,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let fingerprint = store::key("handoff", &(identities, advance, merge))?;
        self.write(|tx| {
            let old = tx.query_row(
                "SELECT fingerprint, receipts FROM messaging_handoffs WHERE connection = ?1 AND id = ?2",
                params![connection, id],
                |row| Ok((row.get::<_,String>(0)?, store::decode::<Vec<IntakeReceipt>>(row, 1)?)),
            ).optional()?;
            if let Some((stored, mut receipts)) = old {
                if stored != fingerprint {
                    return Ok(Err(GatewayError::Conflict));
                }
                for receipt in &mut receipts {
                    if let Some(current) = store::intake_receipt(tx, connection, &receipt.id)?
                        && receipt.disposition != Disposition::Conflict
                    {
                        *receipt = current;
                    }
                    receipt.repeated = true;
                }
                let checkpoint = advance.map(|a| store::read_checkpoint(tx, connection, &a.stream))
                    .transpose()?.flatten();
                return Ok(Ok(IntakeResult { receipts, checkpoint }));
            }
            if !store::check_advance(tx, connection, advance)? {
                return Ok(Err(GatewayError::CheckpointChanged));
            }
            for (candidate, _) in &candidates {
                if super::repair::blocks_intake(tx, connection, &candidate.event)? {
                    return Ok(Err(GatewayError::TemporarilyUnavailable));
                }
            }
            let merge_event = if merge && candidates.len() > 1 {
                let mut event = candidates[0].0.event.clone();
                let merged = candidates.iter().filter_map(|(candidate,_)| match &candidate.event.body {
                    InboundBody::Message(message) => Some(message.text.as_str()),
                    _ => None,
                }).collect::<Vec<_>>().join("\n");
                if merged.len() > super::runtime::MAX_TEXT {
                    return Ok(Err(GatewayError::Invalid));
                }
                event.body = InboundBody::Message(Message::text(merged));
                Some(event)
            } else {
                None
            };
            let mut receipts = Vec::with_capacity(candidates.len());
            for (candidate, preparation) in candidates {
                let prepare = preparation.is_some()
                    && preparation.as_ref().is_some_and(|p| p.expires_ms > chrono::Utc::now().timestamp_millis());
                let candidate = if preparation.is_some() && !prepare {
                    Candidate { decision: Err(Disposition::Expired), ..candidate }
                } else {
                    candidate
                };
                let receipt = store::append(tx, connection, revision, candidate, prepare)?;
                if let Some(preparation) = preparation
                    && receipt.disposition == Disposition::Deferred && !receipt.repeated
                {
                    tx.execute(
                        "INSERT INTO messaging_preparation(connection,event,token,purpose,expires)
                         VALUES (?1,?2,?3,?4,?5)",
                        params![connection, receipt.id, preparation.token,
                            serde_json::to_string(&preparation.purpose)?, preparation.expires_ms],
                    )?;
                }
                receipts.push(receipt);
            }
            if let Some(event) = merge_event
                && receipts.iter().all(|receipt| !receipt.repeated && receipt.disposition == Disposition::Accepted
                    && receipt.route == receipts[0].route)
            {
                tx.execute(
                    "UPDATE messaging_ingress SET input = ?3 WHERE connection = ?1 AND event = ?2",
                    params![connection,event.id,serde_json::to_string(&event)?],
                )?;
                let disposition = serde_json::to_string(&Disposition::Coalesced)?;
                for receipt in &mut receipts[1..] {
                    tx.execute(
                        "UPDATE messaging_ingress SET input = NULL,stage = 'done',disposition = ?3,reason = ?4
                         WHERE connection = ?1 AND event = ?2",
                        params![connection,receipt.id,disposition,event.id],
                    )?;
                    receipt.disposition = Disposition::Coalesced;
                }
            }
            let checkpoint = store::write_advance(tx, connection, advance)?;
            tx.execute(
                "INSERT INTO messaging_handoffs(connection,id,fingerprint,receipts) VALUES (?1,?2,?3,?4)",
                params![connection, id, fingerprint, serde_json::to_string(&receipts)?],
            )?;
            Ok(Ok(IntakeResult { receipts, checkpoint }))
        })
    }

    fn preparations(&self, connection: &str) -> Result<Vec<DeferredInput>> {
        self.read(|db| {
            let mut query = db.prepare(
                "SELECT i.input,p.token,p.purpose,p.expires FROM messaging_preparation p
                 JOIN messaging_ingress i USING(connection,event)
                 WHERE p.connection = ?1 AND i.stage = 'preparing' AND p.expires > ?2
                 ORDER BY i.position LIMIT ?3",
            )?;
            Ok(query
                .query_map(
                    params![
                        connection,
                        chrono::Utc::now().timestamp_millis(),
                        MAX_BATCH as i64
                    ],
                    |row| {
                        Ok(DeferredInput {
                            event: store::decode(row, 0)?,
                            preparation: Preparation {
                                token: row.get(1)?,
                                purpose: store::decode(row, 2)?,
                                expires_ms: row.get(3)?,
                            },
                        })
                    },
                )?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    fn preparation(&self, connection: &str, event: &str) -> Result<Option<Staged>> {
        self.read(|db| Ok(db.query_row(
            "SELECT p.token,p.expires,p.finished,i.route,i.disposition,i.input FROM messaging_preparation p
             JOIN messaging_ingress i USING(connection,event) WHERE p.connection = ?1 AND p.event = ?2",
            params![connection,event],
            |row| Ok(Staged { token: row.get(0)?, expires_ms: row.get(1)?, finished: row.get(2)?,
                receipt: IntakeReceipt { id: event.to_owned(), route: row.get(3)?,
                    disposition: store::decode(row, 4)?, repeated: false },
                event: store::optional_decode(row, 5)?,
            }),
        ).optional()?))
    }

    fn finish_preparation(
        &self,
        connection: &str,
        id: &str,
        token: &str,
        fingerprint: &str,
        candidate: Candidate,
    ) -> Result<IntakeReceipt> {
        self.write(|tx| {
            let old = store::intake_receipt(tx, connection, id)?
                .ok_or_else(|| anyhow::anyhow!("Deferred input disappeared"))?;
            if old.disposition != Disposition::Deferred {
                return Ok(old);
            }
            let finished = tx.execute(
                "UPDATE messaging_preparation SET finished = ?4
                 WHERE connection = ?1 AND event = ?2 AND token = ?3 AND finished IS NULL",
                params![connection, id, token, fingerprint],
            )?;
            ensure!(finished == 1, "Preparation changed before commit");
            let decision =
                store::attachment_decision(tx, connection, &candidate.event, candidate.decision)?;
            let (disposition, body, stage) = match decision {
                Ok(route) if old.route.as_deref() == Some(route.id.as_str()) => (
                    Disposition::Accepted,
                    Some(serde_json::to_string(&candidate.event)?),
                    "pending",
                ),
                Ok(_) => (Disposition::Forbidden, None, "refused"),
                Err(disposition) => (disposition, None, "refused"),
            };
            tx.execute(
                "UPDATE messaging_ingress SET stage = ?3, disposition = ?4, input = ?5
                 WHERE connection = ?1 AND event = ?2 AND stage = 'preparing'",
                params![
                    connection,
                    id,
                    stage,
                    serde_json::to_string(&disposition)?,
                    body
                ],
            )?;
            Ok(IntakeReceipt { disposition, ..old })
        })
    }

    pub(super) fn expire_preparations(&self, connection: &str, now: i64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE messaging_ingress SET stage = 'refused', reason = 'preparation_expired', input = NULL, disposition = ?3
                 WHERE connection = ?1 AND stage = 'preparing' AND event IN
                   (SELECT event FROM messaging_preparation WHERE connection = ?1 AND expires <= ?2)",
                params![connection, now, serde_json::to_string(&Disposition::Expired)?],
            )?;
            Ok(())
        })
    }
}
