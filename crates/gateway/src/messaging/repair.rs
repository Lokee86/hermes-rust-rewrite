use super::{
    runtime::{Ingress, MAX_BATCH, blocking, db, valid_advance, valid_id},
    store::{self, Candidate, Store},
    types::*,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_repairs (
    connection TEXT NOT NULL, id TEXT NOT NULL, stream TEXT NOT NULL, room TEXT NOT NULL,
    window TEXT NOT NULL, transport_scope TEXT NOT NULL, page TEXT,
    staged INTEGER NOT NULL DEFAULT 0, disposition TEXT,
    PRIMARY KEY(connection,id)
);
CREATE UNIQUE INDEX IF NOT EXISTS messaging_active_repairs ON messaging_repairs(connection,room)
    WHERE disposition IS NULL;
CREATE TABLE IF NOT EXISTS messaging_repair_items (
    connection TEXT NOT NULL, repair TEXT NOT NULL, id TEXT NOT NULL,
    source_order INTEGER NOT NULL, fingerprint TEXT NOT NULL, input TEXT NOT NULL,
    PRIMARY KEY(connection,repair,id)
);
CREATE TABLE IF NOT EXISTS messaging_repair_pages (
    connection TEXT NOT NULL, repair TEXT NOT NULL, page TEXT NOT NULL, fingerprint TEXT NOT NULL,
    PRIMARY KEY(connection,repair,page)
);
";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepairWindow {
    pub id: String,
    pub stream: String,
    pub room: Room,
    pub last_committed: String,
    pub expires_ms: i64,
    pub limit: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepairItem {
    pub source_order: i64,
    pub event: InboundEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepairPage {
    pub id: String,
    pub previous: Option<String>,
    pub items: Vec<RepairItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairProgress {
    pub window: RepairWindow,
    pub last_page: Option<String>,
    pub staged: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepairResult {
    pub receipts: Vec<IntakeReceipt>,
    pub checkpoint: Option<String>,
    pub gap: bool,
}

impl Ingress {
    pub async fn begin_repair(&self, window: RepairWindow) -> Result<RepairProgress, GatewayError> {
        if !valid_id(&window.id)
            || !valid_id(&window.room.chat)
            || window
                .room
                .thread
                .as_deref()
                .is_some_and(|id| !valid_id(id))
            || !(1..=4096).contains(&window.limit)
            || window.expires_ms <= chrono::Utc::now().timestamp_millis()
            || !valid_advance(Some(&Advance {
                stream: window.stream.clone(),
                previous: None,
                checkpoint: window.last_committed.clone(),
            }))
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |state| {
                if !scope.capabilities().text_intake
                    || state.policy.room_policy(&window.room).is_none()
                {
                    return Err(GatewayError::Forbidden);
                }
                let id = scope.scoped_transport_key("repair-window", &window.id)?;
                let stream = scope.scoped_transport_key("checkpoint-stream", &window.stream)?;
                db(scope.store.begin_repair(
                    &scope.connection,
                    &id,
                    &stream,
                    &scope.transport_scope()?,
                    window,
                ))?
            })
        })
        .await
    }

    pub async fn pending_repairs(&self) -> Result<Vec<RepairProgress>, GatewayError> {
        let scope = self.clone();
        blocking(move || scope.checked(true, |_| db(scope.store.repairs(&scope.connection)))).await
    }

    /// Stage bounded recovered and live observations in the same window. Its
    /// ordinary route is fenced until a durable ordered merge or gap result.
    pub async fn stage_repair_page(
        &self,
        window: String,
        page: RepairPage,
    ) -> Result<RepairProgress, GatewayError> {
        if !valid_id(&window)
            || !valid_id(&page.id)
            || page.items.len() > MAX_BATCH
            || page.previous.as_deref().is_some_and(|id| !valid_id(id))
            || page.items.iter().any(|item| {
                !super::runtime::valid_input_id(&item.event.id) || !matches!(&item.event.body, InboundBody::Message(message) if super::runtime::valid_message(message))
            })
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |state| {
                let id = scope.scoped_transport_key("repair-window", &window)?;
                let progress =
                    db(scope.store.repair(&scope.connection, &id))?.ok_or(GatewayError::Invalid)?;
                if state.policy.room_policy(&progress.window.room).is_none()
                    || page
                        .items
                        .iter()
                        .any(|item| item.event.room != progress.window.room)
                {
                    return Err(GatewayError::Forbidden);
                }
                let mut protection = Vec::with_capacity(page.items.len());
                for item in &page.items {
                    let event = &item.event;
                    let identity = scope.authorized_identity(&state.policy, &event.room, &event.sender)?;
                    let actor = identity.as_ref().map_or(event.sender.id.as_str(), |(sender, _)| sender.id.as_str());
                    let class = scope.check_source_protection(&state.policy, &event.room, actor, &event.id)?;
                    if class.is_some() && identity.is_none() {
                        return Err(GatewayError::Forbidden);
                    }
                    protection.push(class.map(|class| (actor.to_owned(), class)));
                }
                db(scope
                    .store
                    .stage_repair_page(&scope.connection, &id, progress, page, protection))?
            })
        })
        .await
    }

    /// End with a qualified provider checkpoint, or release the verified
    /// subset while retaining an explicit uncovered interval.
    pub async fn finish_repair(
        &self,
        id: String,
        advance: Option<Advance>,
    ) -> Result<RepairResult, GatewayError> {
        if !valid_id(&id) || !valid_advance(advance.as_ref()) {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        let result = blocking(move || {
            scope.checked(true, |state| {
                let id = scope.scoped_transport_key("repair-window", &id)?;
                if let Some(result) = db(scope.store.repair_result(&scope.connection, &id))? {
                    if result.gap != advance.is_none()
                        || advance
                            .as_ref()
                            .is_some_and(|a| Some(&a.checkpoint) != result.checkpoint.as_ref())
                    {
                        return Err(GatewayError::Conflict);
                    }
                    return Ok(result);
                }
                let progress =
                    db(scope.store.repair(&scope.connection, &id))?.ok_or(GatewayError::Invalid)?;
                let window = &progress.window;
                if let Some(advance) = &advance
                    && (advance.stream != window.stream
                        || advance.previous.as_deref() != Some(window.last_committed.as_str())
                        || window.expires_ms <= chrono::Utc::now().timestamp_millis())
                {
                    return Err(GatewayError::CheckpointChanged);
                }
                let mut candidates = Vec::new();
                for event in db(scope.store.repair_items(&scope.connection, &id))? {
                    let decision = scope.route_event(&state.policy, &event)?;
                    if decision.as_ref().err() == Some(&Disposition::ProtectionUnavailable) {
                        return Err(GatewayError::ProtectionUnavailable);
                    }
                    candidates.push(scope.intake_candidate(&state.policy, event, decision, None)?);
                }
                let advance = scope.scoped_advance(advance)?;
                db(scope.store.finish_repair(
                    &scope.connection,
                    state.revision,
                    &id,
                    candidates,
                    advance.as_ref(),
                ))?
            })
        })
        .await?;
        if result.gap {
            // The gap is already durable. A concurrent stop can fence this
            // mirror update without making the committed result ambiguous.
            let _ = self.report_gap();
        }
        self.notify();
        Ok(result)
    }
}

fn progress(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepairProgress> {
    Ok(RepairProgress {
        window: store::decode(row, 0)?,
        last_page: row.get(1)?,
        staged: row.get(2)?,
    })
}

pub(super) fn blocks_intake(
    db: &Connection,
    connection: &str,
    event: &InboundEvent,
) -> Result<bool> {
    if !matches!(event.body, InboundBody::Message(_)) {
        return Ok(false);
    }
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM messaging_repairs WHERE connection = ?1 AND room = ?2 AND disposition IS NULL)",
        params![connection,serde_json::to_string(&event.room)?],
        |row| row.get(0),
    )?)
}

impl Store {
    pub(super) fn retire_foreign_repairs(&self, connection: &str, scope: &str) -> Result<()> {
        self.write(|tx| {
            let gap = RepairResult {
                receipts: Vec::new(),
                checkpoint: None,
                gap: true,
            };
            let retired = tx.execute(
                "UPDATE messaging_repairs SET disposition=?3
                 WHERE connection=?1 AND transport_scope<>?2 AND disposition IS NULL",
                params![connection, scope, serde_json::to_string(&gap)?],
            )?;
            if retired > 0 {
                tx.execute(
                    "INSERT OR IGNORE INTO messaging_gaps(connection) VALUES (?1)",
                    [connection],
                )?;
            }
            Ok(())
        })
    }

    fn repair(&self, connection: &str, id: &str) -> Result<Option<RepairProgress>> {
        self.read(|db| Ok(db.query_row(
            "SELECT window,page,staged FROM messaging_repairs WHERE connection = ?1 AND id = ?2 AND disposition IS NULL",
            params![connection,id], progress,
        ).optional()?))
    }

    fn repairs(&self, connection: &str) -> Result<Vec<RepairProgress>> {
        self.read(|db| {
            let mut query = db.prepare(
                "SELECT window,page,staged FROM messaging_repairs WHERE connection = ?1 AND disposition IS NULL
                 ORDER BY rowid LIMIT 128",
            )?;
            Ok(query.query_map([connection], progress)?.collect::<rusqlite::Result<_>>()?)
        })
    }

    fn begin_repair(
        &self,
        connection: &str,
        id: &str,
        stream: &str,
        scope: &str,
        window: RepairWindow,
    ) -> Result<Result<RepairProgress, GatewayError>> {
        self.write(|tx| {
            if let Some(old) = tx.query_row(
                "SELECT window,page,staged FROM messaging_repairs WHERE connection = ?1 AND id = ?2",
                params![connection,id],progress,
            ).optional()? {
                return Ok(if old.window == window { Ok(old) } else { Err(GatewayError::Conflict) });
            }
            if store::read_checkpoint(tx, connection, stream)?.as_deref() != Some(window.last_committed.as_str()) {
                return Ok(Err(GatewayError::CheckpointChanged));
            }
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO messaging_repairs(connection,id,stream,room,window,transport_scope)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![connection,id,stream,serde_json::to_string(&window.room)?,serde_json::to_string(&window)?,scope],
            )?;
            Ok(if inserted == 1 { Ok(RepairProgress {window,last_page:None,staged:0}) }
                else { Err(GatewayError::Conflict) })
        })
    }

    fn stage_repair_page(
        &self,
        connection: &str,
        id: &str,
        mut state: RepairProgress,
        page: RepairPage,
        protection: Vec<Option<(String, String)>>,
    ) -> Result<Result<RepairProgress, GatewayError>> {
        let identities = page
            .items
            .iter()
            .map(|item| {
                Ok((
                    item.source_order,
                    &item.event.id,
                    store::fingerprint(&item.event)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let fingerprint = store::key("repair-page", &(&page.previous, identities))?;
        self.write(|tx| {
            if let Some(old) = tx.query_row(
                "SELECT fingerprint FROM messaging_repair_pages WHERE connection = ?1 AND repair = ?2 AND page = ?3",
                params![connection,id,page.id], |row| row.get::<_,String>(0),
            ).optional()? {
                return Ok(if old == fingerprint { Ok(state) } else { Err(GatewayError::Conflict) });
            }
            if state.last_page != page.previous || chrono::Utc::now().timestamp_millis() >= state.window.expires_ms {
                return Ok(Err(GatewayError::CheckpointChanged));
            }
            // Returning an error from the transaction rolls back the entire
            // page. A bad or overflowing page cannot partially advance it.
            let write = (|| -> Result<(), RepairWrite> {
                for (row, protection) in page.items.into_iter().zip(protection) {
                    let hash = store::fingerprint(&row.event)?;
                    if let Some(old) = tx.query_row(
                        "SELECT source_order,fingerprint FROM messaging_repair_items WHERE connection = ?1 AND repair = ?2 AND id = ?3",
                        params![connection,id,row.event.id], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)),
                    ).optional()? {
                        if old != (row.source_order, hash) {
                            return Err(RepairWrite::Conflict);
                        }
                        continue;
                    }
                    if state.staged == state.window.limit {
                        return Err(RepairWrite::Limit);
                    }
                    tx.execute(
                        "INSERT INTO messaging_repair_items(connection,repair,id,source_order,fingerprint,input)
                         VALUES (?1,?2,?3,?4,?5,?6)",
                        params![connection,id,row.event.id,row.source_order,hash,serde_json::to_string(&row.event).map_err(anyhow::Error::from)?],
                    )?;
                    if let Some((actor, class)) = protection {
                        super::protection::pin_input(tx, connection, &row.event.id, &row.event.room, &actor, &class)?;
                    }
                    state.staged += 1;
                }
                Ok(())
            })();
            if let Err(error) = write {
                return Err(anyhow::Error::new(error));
            }
            ensure!(tx.execute(
                "UPDATE messaging_repairs SET page = ?3,staged = ?4 WHERE connection = ?1 AND id = ?2 AND page IS ?5
                   AND disposition IS NULL",
                params![connection,id,page.id,state.staged,state.last_page],
            )? == 1, "Repair window changed");
            tx.execute(
                "INSERT INTO messaging_repair_pages(connection,repair,page,fingerprint) VALUES (?1,?2,?3,?4)",
                params![connection,id,page.id,fingerprint],
            )?;
            state.last_page = Some(page.id);
            Ok(Ok(state))
        }).or_else(|error| match error.downcast_ref::<RepairWrite>() {
            Some(RepairWrite::Limit) => Ok(Err(GatewayError::Invalid)),
            Some(RepairWrite::Conflict) => Ok(Err(GatewayError::Conflict)),
            _ => Err(error),
        })
    }

    fn repair_items(&self, connection: &str, id: &str) -> Result<Vec<InboundEvent>> {
        self.read(|db| {
            let mut query = db.prepare(
                "SELECT input FROM messaging_repair_items WHERE connection = ?1 AND repair = ?2 ORDER BY source_order,id"
            )?;
            Ok(query.query_map(params![connection,id], |row| store::decode(row,0))?.collect::<rusqlite::Result<_>>()?)
        })
    }

    fn repair_result(&self, connection: &str, id: &str) -> Result<Option<RepairResult>> {
        self.read(|db| Ok(db.query_row(
            "SELECT disposition FROM messaging_repairs WHERE connection = ?1 AND id = ?2 AND disposition IS NOT NULL",
            params![connection,id], |row| store::decode(row,0),
        ).optional()?))
    }

    fn finish_repair(
        &self,
        connection: &str,
        revision: i64,
        id: &str,
        candidates: Vec<Candidate>,
        advance: Option<&Advance>,
    ) -> Result<Result<RepairResult, GatewayError>> {
        self.write(|tx| {
            if !store::check_advance(tx, connection, advance)? {
                return Ok(Err(GatewayError::CheckpointChanged));
            }
            let mut receipts = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                receipts.push(store::append(tx, connection, revision, candidate, false)?);
            }
            let result = RepairResult {
                receipts,
                gap: advance.is_none(),
                checkpoint: store::write_advance(tx, connection, advance)?,
            };
            ensure!(tx.execute(
                "UPDATE messaging_repairs SET disposition = ?3 WHERE connection = ?1 AND id = ?2
                 AND disposition IS NULL",
                params![connection,id,serde_json::to_string(&result)?],
            )? == 1, "Repair was already finalized");
            tx.execute(
                "DELETE FROM messaging_repair_items WHERE connection = ?1 AND repair = ?2",
                params![connection, id],
            )?;
            if result.gap {
                tx.execute(
                    "INSERT OR IGNORE INTO messaging_gaps(connection) VALUES (?1)",
                    [connection],
                )?;
            }
            Ok(Ok(result))
        })
    }
}

#[derive(Debug)]
enum RepairWrite {
    Limit,
    Conflict,
    Storage(anyhow::Error),
}

impl std::fmt::Display for RepairWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit => f.write_str("Repair exceeded its authorized item budget"),
            Self::Conflict => f.write_str("Repair item was repeated with different evidence"),
            Self::Storage(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for RepairWrite {}

impl From<anyhow::Error> for RepairWrite {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

impl From<rusqlite::Error> for RepairWrite {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}
