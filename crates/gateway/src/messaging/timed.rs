use super::{
    Disposition, GatewayError, InboundBody, InboundEvent, Ingress, IntakeReceipt,
    repair::blocks_intake,
    runtime::{MAX_BATCH, blocking, db, valid_id, valid_input_id},
    store::{self, Candidate, Store},
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_OVERLAP_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const MAX_BOUNDARY_EVENTS: usize = 256;

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_time_checkpoints (
    connection TEXT NOT NULL, stream TEXT NOT NULL, cursor TEXT NOT NULL,
    PRIMARY KEY(connection, stream)
);
";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TimeCheckpoint {
    pub discovery_baseline_ms: i64,
    pub overlap_ms: i64,
    pub through_ms: i64,
    pub boundary_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimedEvent {
    pub timestamp_ms: i64,
    pub event: InboundEvent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeCoverage {
    /// Verified coverage through this inclusive boundary.
    Complete {
        through_ms: i64,
    },
    Saturated,
    Unverified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeIntake {
    pub checkpoint: TimeCheckpoint,
    pub receipts: Vec<IntakeReceipt>,
    pub gap: bool,
}

impl Ingress {
    /// Record the configured initial boundary separately from poll progress.
    pub async fn open_time_checkpoint(
        &self,
        stream: String,
        discovery_baseline_ms: i64,
        overlap_ms: i64,
    ) -> Result<TimeCheckpoint, GatewayError> {
        if !valid_id(&stream)
            || discovery_baseline_ms < 0
            || !(0..=MAX_OVERLAP_MS).contains(&overlap_ms)
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |_| {
                if !scope.capabilities().text_intake {
                    return Err(GatewayError::Unsupported);
                }
                let stream = scope.scoped_transport_key("time-checkpoint", &stream)?;
                let initial = TimeCheckpoint {
                    discovery_baseline_ms,
                    overlap_ms,
                    through_ms: discovery_baseline_ms,
                    boundary_ids: BTreeSet::new(),
                };
                db(scope
                    .store
                    .open_time_checkpoint(&scope.connection, &stream, initial))?
            })
        })
        .await
    }

    /// Submit a bounded inclusive poll page. Saturation and unverified clock
    /// coverage leave the last proven checkpoint intact.
    pub async fn submit_timed(
        &self,
        stream: String,
        previous: TimeCheckpoint,
        mut items: Vec<TimedEvent>,
        coverage: TimeCoverage,
    ) -> Result<TimeIntake, GatewayError> {
        if !valid_id(&stream) || items.len() > MAX_BATCH
            || previous.boundary_ids.len() > MAX_BOUNDARY_EVENTS
            || matches!(coverage, TimeCoverage::Complete { through_ms } if through_ms < previous.through_ms)
            || items.iter().any(|item| {
                item.timestamp_ms < 0 || !valid_input_id(&item.event.id)
                    || !matches!(item.event.body, InboundBody::Message(_))
                    || matches!(coverage, TimeCoverage::Complete { through_ms } if item.timestamp_ms > through_ms)
            })
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        let result = blocking(move || {
            scope.checked(true, |state| {
                if !scope.capabilities().text_intake {
                    return Err(GatewayError::Unsupported);
                }
                let stream = scope.scoped_transport_key("time-checkpoint", &stream)?;
                items.sort_by(|a, b| {
                    (a.timestamp_ms, &a.event.id).cmp(&(b.timestamp_ms, &b.event.id))
                });
                let floor = previous
                    .through_ms
                    .saturating_sub(previous.overlap_ms)
                    .max(previous.discovery_baseline_ms);
                let mut next = previous.clone();
                if let TimeCoverage::Complete { through_ms } = coverage
                    && through_ms > previous.through_ms
                {
                    next.through_ms = through_ms;
                    next.boundary_ids.clear();
                }
                let mut candidates = Vec::with_capacity(items.len());
                for item in items {
                    let decision = if item.timestamp_ms < floor {
                        Err(Disposition::Historical)
                    } else {
                        scope.route_event(&state.policy, &item.event)?
                    };
                    if item.timestamp_ms == next.through_ms {
                        next.boundary_ids.insert(item.event.id.clone());
                    }
                    candidates.push(scope.intake_candidate(&state.policy, item.event, decision, None)?);
                }
                let gap = !matches!(coverage, TimeCoverage::Complete { .. })
                    || next.boundary_ids.len() > MAX_BOUNDARY_EVENTS;
                if gap {
                    next.clone_from(&previous);
                }
                db(scope.store.commit_time_page(
                    &scope.connection,
                    &stream,
                    state.revision,
                    previous,
                    next,
                    candidates,
                    gap,
                ))?
            })
        })
        .await?;
        if result.gap {
            let _ = self.report_gap();
        }
        self.notify();
        Ok(result)
    }
}

fn checkpoint(
    db: &rusqlite::Connection,
    connection: &str,
    stream: &str,
) -> Result<Option<TimeCheckpoint>> {
    Ok(db
        .query_row(
            "SELECT cursor FROM messaging_time_checkpoints WHERE connection=?1 AND stream=?2",
            params![connection, stream],
            |row| store::decode(row, 0),
        )
        .optional()?)
}

impl Store {
    fn open_time_checkpoint(
        &self,
        connection: &str,
        stream: &str,
        initial: TimeCheckpoint,
    ) -> Result<Result<TimeCheckpoint, GatewayError>> {
        self.write(|tx| {
            if let Some(previous) = checkpoint(tx, connection, stream)? {
                return Ok(if previous.discovery_baseline_ms == initial.discovery_baseline_ms
                    && previous.overlap_ms == initial.overlap_ms {
                    Ok(previous)
                } else {
                    Err(GatewayError::Conflict)
                });
            }
            tx.execute(
                "INSERT INTO messaging_time_checkpoints(connection,stream,cursor) VALUES (?1,?2,?3)",
                params![connection, stream, serde_json::to_string(&initial)?],
            )?;
            Ok(Ok(initial))
        })
    }

    fn commit_time_page(
        &self,
        connection: &str,
        stream: &str,
        revision: i64,
        previous: TimeCheckpoint,
        next: TimeCheckpoint,
        candidates: Vec<Candidate>,
        gap: bool,
    ) -> Result<Result<TimeIntake, GatewayError>> {
        self.write(|tx| {
            if checkpoint(tx, connection, stream)?.as_ref() != Some(&previous) {
                return Ok(Err(GatewayError::CheckpointChanged));
            }
            for candidate in &candidates {
                if blocks_intake(tx, connection, &candidate.event)? {
                    return Ok(Err(GatewayError::CheckpointChanged));
                }
            }
            let mut receipts = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                receipts.push(store::append(tx, connection, revision, candidate, false)?);
            }
            if gap {
                tx.execute("INSERT OR IGNORE INTO messaging_gaps(connection) VALUES (?1)", [connection])?;
            } else if next != previous {
                tx.execute("UPDATE messaging_time_checkpoints SET cursor=?3 WHERE connection=?1 AND stream=?2",
                    params![connection,stream,serde_json::to_string(&next)?])?;
            }
            Ok(Ok(TimeIntake { checkpoint: next, receipts, gap }))
        })
    }
}
