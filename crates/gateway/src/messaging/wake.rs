use super::{
    runtime::{Ingress, blocking, db, valid_id},
    store::{self, Candidate, Route, Store},
    types::*,
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_wake_targets (
    connection TEXT PRIMARY KEY, target TEXT NOT NULL, instructions TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS messaging_wake_activity (
    ordinal INTEGER PRIMARY KEY, connection TEXT NOT NULL, event TEXT NOT NULL, value TEXT NOT NULL,
    UNIQUE(connection,event)
);
CREATE TABLE IF NOT EXISTS messaging_wake_activity_drops (
    connection TEXT PRIMARY KEY, count INTEGER NOT NULL
);
";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WakeTarget {
    pub target: String,
    pub instructions: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WakeActivity {
    pub id: String,
    pub session: String,
    pub status: WakeStatus,
    pub time_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeStatus {
    Thinking,
    Complete,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActivityDrain {
    pub events: Vec<WakeActivity>,
    pub dropped: u64,
}

impl WakeTarget {
    pub(super) fn route(&self, connection: &str) -> Result<Route> {
        Route::new(
            connection,
            Room {
                kind: RoomKind::Direct,
                chat: self.target.clone(),
                thread: None,
            },
            None,
        )
    }
}

impl Ingress {
    /// Accept a content-free trigger for the owner-selected target. No actor,
    /// room, message body or transient transport attempt ID is accepted here.
    pub async fn wake(&self, id: String) -> Result<IntakeReceipt, GatewayError> {
        if !self.capabilities().wake {
            return Err(GatewayError::Unsupported);
        }
        if !valid_id(&id) {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        let receipt = blocking(move || {
            scope.checked(true, |state| {
                let target = db(scope.store.wake_target(&scope.connection))?
                    .ok_or(GatewayError::TemporarilyUnavailable)?;
                db(scope
                    .store
                    .stage_wake(&scope.connection, state.revision, target, id))
            })
        })
        .await?;
        self.notify();
        Ok(receipt)
    }

    /// Best-effort, bounded bridge telemetry. The bridge's authenticated
    /// generation can drain only its own activity.
    pub async fn drain_activity(&self, limit: usize) -> Result<ActivityDrain, GatewayError> {
        if !self.capabilities().wake {
            return Err(GatewayError::Unsupported);
        }
        if !(1..=128).contains(&limit) {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |_| {
                db(scope.store.drain_activity(&scope.connection, limit))
            })
        })
        .await
    }
}

impl Store {
    pub(super) fn wake_target(&self, connection: &str) -> Result<Option<WakeTarget>> {
        self.read(|db| {
            Ok(db
                .query_row(
                    "SELECT target,instructions FROM messaging_wake_targets WHERE connection = ?1",
                    [connection],
                    |row| {
                        Ok(WakeTarget {
                            target: row.get(0)?,
                            instructions: row.get(1)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub(super) fn set_wake_target(&self, connection: &str, target: &WakeTarget) -> Result<()> {
        ensure!(
            valid_id(&target.target)
                && !target.instructions.trim().is_empty()
                && target.instructions.len() <= 4096,
            "Invalid wake target"
        );
        self.write(|tx| {
            ensure!(tx.execute(
                "INSERT INTO messaging_wake_targets(connection,target,instructions) VALUES (?1,?2,?3)
                 ON CONFLICT(connection) DO UPDATE SET instructions = excluded.instructions
                 WHERE target = excluded.target",
                params![connection,target.target,target.instructions],
            )? == 1, "The configured wake session already has a different binding");
            Ok(())
        })
    }

    fn stage_wake(
        &self,
        connection: &str,
        revision: i64,
        target: WakeTarget,
        id: String,
    ) -> Result<IntakeReceipt> {
        let route = target.route(connection)?;
        let event = InboundEvent {
            id,
            room: route.room.clone(),
            sender: Sender {
                id: "bridge".into(),
                roles: Default::default(),
            },
            reply_anchor: None,
            mentioned: true,
            body: InboundBody::Wake,
        };
        self.write(|tx| {
            let covering = tx.query_row(
                "SELECT event FROM messaging_ingress WHERE connection = ?1 AND route = ?2
                   AND stage = 'pending' ORDER BY position LIMIT 1",
                params![connection,route.id], |row| row.get::<_,String>(0),
            ).optional()?;
            let mut receipt = store::append(tx, connection, revision, Candidate { event, decision: Ok(route), protection: None }, false)?;
            if !receipt.repeated && receipt.disposition == Disposition::Accepted
                && let Some(covering) = covering
            {
                receipt.disposition = Disposition::Coalesced;
                tx.execute(
                    "UPDATE messaging_ingress SET disposition = ?3, stage = 'done', input = NULL, reason = ?4
                     WHERE connection = ?1 AND event = ?2",
                    params![connection,receipt.id,serde_json::to_string(&receipt.disposition)?,covering],
                )?;
            }
            Ok(receipt)
        })
    }

    pub(super) fn reserve_wake(&self, route: &Route, id: &str) -> Result<()> {
        self.write(|tx| {
            ensure!(tx.execute(
                "UPDATE messaging_ingress SET stage = 'admitting' WHERE route = ?1 AND event = ?2
                 AND stage IN ('pending','admitting')",
                params![route.id,id],
            )? == 1, "The wake is no longer eligible");
            Ok(())
        })
    }

    pub(super) fn wake_activity(
        &self,
        connection: &str,
        input: &str,
        session: &str,
        status: WakeStatus,
    ) -> Result<()> {
        let id = store::key("wake-activity", &(connection, input, status))?;
        let event = WakeActivity {
            id,
            session: session.to_owned(),
            status,
            time_ms: chrono::Utc::now().timestamp_millis(),
        };
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO messaging_wake_activity(connection,event,value) VALUES (?1,?2,?3)",
                params![connection,event.id,serde_json::to_string(&event)?],
            )?;
            let dropped = tx.execute(
                "DELETE FROM messaging_wake_activity WHERE ordinal IN (
                   SELECT ordinal FROM messaging_wake_activity WHERE connection = ?1
                   ORDER BY ordinal DESC LIMIT -1 OFFSET 500
                 )", [connection],
            )?;
            if dropped > 0 {
                tx.execute(
                    "INSERT INTO messaging_wake_activity_drops(connection,count) VALUES (?1,?2)
                     ON CONFLICT(connection) DO UPDATE SET count = count + excluded.count",
                    params![connection,dropped as i64],
                )?;
            }
            Ok(())
        })
    }

    fn drain_activity(&self, connection: &str, limit: usize) -> Result<ActivityDrain> {
        self.write(|tx| {
            let mut query = tx.prepare(
                "SELECT ordinal,value FROM messaging_wake_activity WHERE connection = ?1 ORDER BY ordinal LIMIT ?2"
            )?;
            let batch = query.query_map(params![connection,limit as i64], |row| {
                Ok((row.get::<_,i64>(0)?, store::decode::<WakeActivity>(row, 1)?))
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some((end, _)) = batch.last() {
                tx.execute(
                    "DELETE FROM messaging_wake_activity WHERE connection = ?1 AND ordinal <= ?2",
                    params![connection,end],
                )?;
            }
            let dropped = tx.query_row(
                "DELETE FROM messaging_wake_activity_drops WHERE connection = ?1 RETURNING count",
                [connection], |row| row.get::<_,i64>(0),
            ).optional()?.unwrap_or(0);
            Ok(ActivityDrain { events: batch.into_iter().map(|(_,event)| event).collect(), dropped: dropped.max(0) as u64 })
        })
    }
}
