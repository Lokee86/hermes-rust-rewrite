use super::types::*;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{path::Path, sync::Mutex};

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_routes (
    id TEXT PRIMARY KEY, connection TEXT NOT NULL, room TEXT NOT NULL,
    session TEXT, blocked INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS messaging_checkpoints (
    connection TEXT NOT NULL, stream TEXT NOT NULL, value TEXT NOT NULL,
    PRIMARY KEY(connection, stream)
);
CREATE TABLE IF NOT EXISTS messaging_gaps (
    connection TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS messaging_ingress (
    position INTEGER PRIMARY KEY, connection TEXT NOT NULL, event TEXT NOT NULL,
    fingerprint TEXT NOT NULL, route TEXT REFERENCES messaging_routes(id),
    disposition TEXT NOT NULL, input TEXT, control INTEGER NOT NULL,
    policy_revision INTEGER NOT NULL, stage TEXT NOT NULL, reason TEXT,
    conflict INTEGER NOT NULL DEFAULT 0, UNIQUE(connection, event)
);
CREATE INDEX IF NOT EXISTS messaging_inbox_active ON messaging_ingress(connection, control, route, position)
    WHERE stage IN ('pending','admitted','uncertain','blocked');
CREATE TABLE IF NOT EXISTS messaging_media (
    connection TEXT NOT NULL, id TEXT NOT NULL, bytes BLOB NOT NULL,
    PRIMARY KEY(connection, id)
);
CREATE TABLE IF NOT EXISTS messaging_actions (
    id TEXT PRIMARY KEY, connection TEXT NOT NULL, route TEXT NOT NULL REFERENCES messaging_routes(id),
    session TEXT NOT NULL, recipient TEXT NOT NULL, generation INTEGER NOT NULL,
    request TEXT NOT NULL, type TEXT NOT NULL, resolved TEXT
);
CREATE TABLE IF NOT EXISTS messaging_deliveries (
    position INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE,
    connection TEXT NOT NULL, destination TEXT NOT NULL,
    route TEXT NOT NULL REFERENCES messaging_routes(id), intent TEXT NOT NULL,
    sender TEXT, stage TEXT NOT NULL, reason TEXT
);
CREATE INDEX IF NOT EXISTS messaging_outbox_active ON messaging_deliveries(connection, destination, position)
    WHERE stage IN ('pending','planned');
CREATE TABLE IF NOT EXISTS messaging_parts (
    delivery TEXT NOT NULL REFERENCES messaging_deliveries(id), ordinal INTEGER NOT NULL,
    body TEXT NOT NULL, stage TEXT NOT NULL, attempt INTEGER NOT NULL DEFAULT 0,
    generation TEXT, receipt TEXT, editable_message TEXT, reason TEXT,
    PRIMARY KEY(delivery, ordinal)
);
CREATE TABLE IF NOT EXISTS messaging_cooldowns (
    connection TEXT NOT NULL, scope TEXT NOT NULL, until_ms INTEGER NOT NULL,
    PRIMARY KEY(connection, scope)
);
";

pub(super) struct Store(Mutex<Connection>);

#[derive(Clone)]
pub(super) struct Route {
    pub id: String,
    pub connection: String,
    pub room: Room,
    pub session: Option<String>,
    pub blocked: bool,
    pub revision: i64,
}

impl Route {
    fn from_row(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(start)?,
            connection: row.get(start + 1)?,
            room: decode(row, start + 2)?,
            session: row.get(start + 3)?,
            blocked: row.get(start + 4)?,
            revision: row.get(start + 5)?,
        })
    }

    pub fn new(connection: &str, room: Room, participant: Option<&str>) -> Result<Self> {
        Ok(Self {
            id: key("route", &(connection, &room, participant))?,
            connection: connection.to_owned(),
            room,
            session: None,
            blocked: false,
            revision: 0,
        })
    }
}

pub(super) struct Candidate {
    pub event: InboundEvent,
    pub decision: Result<Route, Disposition>,
}

pub(super) struct PendingInput {
    pub event: InboundEvent,
    pub route: Route,
    pub stage: String,
    pub head: bool,
}

pub(super) struct PendingDelivery {
    pub intent: DeliveryIntent,
    pub route: String,
    pub sender: Option<Sender>,
    pub planned: bool,
}

#[derive(Serialize)]
pub(super) struct DeliveryStatus {
    pub id: String,
    pub connection: String,
    pub route: String,
    pub state: String,
    pub part: Option<u32>,
    pub attempt: Option<u32>,
    pub part_state: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub(super) struct InputStatus {
    pub id: String,
    pub connection: String,
    pub route: Option<String>,
    pub session: Option<String>,
    pub state: String,
    pub reason: Option<String>,
    pub conflict: bool,
}

#[derive(Clone)]
pub(super) struct Action {
    pub id: String,
    pub route: String,
    pub session: String,
    pub recipient: String,
    pub generation: u64,
    pub request: String,
    pub kind: String,
    pub resolved: Option<Response>,
}

pub(super) fn key(kind: &str, parts: &impl Serialize) -> Result<String> {
    use std::fmt::Write;
    let encoded = serde_json::to_vec(parts)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &encoded);
    let mut id = String::from(kind);
    id.push('-');
    for byte in digest.as_ref() {
        write!(id, "{byte:02x}")?;
    }
    Ok(id)
}

pub(super) fn decode<T: DeserializeOwned>(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T> {
    let bytes = row.get_ref(index)?.as_str()?;
    serde_json::from_str(bytes).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

pub(super) fn optional_decode<T: DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    if row.get_ref(index)?.data_type() == rusqlite::types::Type::Null {
        return Ok(None);
    }
    decode(row, index).map(Some)
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let connection = hermes_engine::db::open(root)?;
        connection.execute_batch(TABLES)?;
        connection.execute_batch(super::commands::TABLES)?;
        connection.execute_batch(super::intake::TABLES)?;
        connection.execute_batch(super::repair::TABLES)?;
        connection.execute_batch(super::routes::TABLES)?;
        connection.execute_batch(super::wake::TABLES)?;
        Ok(Self(Mutex::new(connection)))
    }

    pub(super) fn write<T>(&self, change: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = crate::lock(&self.0);
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = change(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    pub(super) fn read<T>(&self, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        read(&crate::lock(&self.0))
    }

    pub fn checkpoint(&self, connection: &str, stream: &str) -> Result<Option<String>> {
        let db = crate::lock(&self.0);
        Ok(read_checkpoint(&db, connection, stream)?)
    }

    pub fn report_gap(&self, connection: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO messaging_gaps (connection) VALUES (?1)",
                [connection],
            )?;
            Ok(())
        })
    }

    pub fn gap(&self, connection: &str) -> Result<bool> {
        let db = crate::lock(&self.0);
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM messaging_gaps WHERE connection = ?1)",
            [connection],
            |row| row.get(0),
        )?)
    }

    pub fn intake(
        &self,
        connection: &str,
        revision: i64,
        candidates: Vec<Candidate>,
        advance: Option<&Advance>,
    ) -> Result<Option<IntakeResult>> {
        self.write(|tx| {
            if !check_advance(tx, connection, advance)? {
                return Ok(None);
            }
            for candidate in &candidates {
                if super::repair::blocks_intake(tx, connection, &candidate.event)? {
                    return Ok(None);
                }
            }
            let mut receipts = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                receipts.push(append(tx, connection, revision, candidate, false)?);
            }
            Ok(Some(IntakeResult {
                receipts,
                checkpoint: write_advance(tx, connection, advance)?,
            }))
        })
    }

    pub fn put_attachment(
        &self,
        connection: &str,
        name: String,
        bytes: &[u8],
    ) -> Result<AttachmentRef> {
        let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
        let id = key("media", &(connection, &name, digest.as_ref()))?;
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO messaging_media VALUES (?1,?2,?3)",
                params![connection, id, bytes],
            )?;
            Ok(AttachmentRef { id, name })
        })
    }

    pub fn attachment(&self, connection: &str, attachment: &AttachmentRef) -> Result<Vec<u8>> {
        let db = crate::lock(&self.0);
        Ok(db.query_row(
            "SELECT bytes FROM messaging_media WHERE connection = ?1 AND id = ?2",
            params![connection, attachment.id],
            |row| row.get(0),
        )?)
    }

    pub fn reply_artifact(
        &self,
        connection: &str,
        generation: &str,
        attempt: &SendAttempt,
        artifact: &AttachmentRef,
    ) -> Result<Option<Vec<u8>>> {
        let db = crate::lock(&self.0);
        let part: Option<DeliveryPart> = db.query_row(
            "SELECT p.body FROM messaging_parts p JOIN messaging_deliveries d ON d.id = p.delivery
             WHERE d.connection = ?1 AND d.id = ?2 AND p.ordinal = ?3 AND p.attempt = ?4
                 AND p.generation = ?5 AND p.stage = 'sending'",
            params![connection, attempt.delivery_id, attempt.part, attempt.attempt, generation],
            |r| decode(r, 0),
        ).optional()?;
        let Some(DeliveryPart {
            body: DeliveryBody::Reply(message),
            ..
        }) = part
        else {
            return Ok(None);
        };
        if !message.attachments.contains(artifact) {
            return Ok(None);
        }
        Ok(db
            .query_row(
                "SELECT bytes FROM messaging_media WHERE connection = ?1 AND id = ?2",
                params![connection, artifact.id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn route(&self, id: &str) -> Result<Option<Route>> {
        let db = crate::lock(&self.0);
        Ok(read_route(&db, id)?)
    }

    pub fn bind(&self, route: &Route, session: &str) -> Result<()> {
        self.write(|tx| {
            ensure!(tx.execute(
                "UPDATE messaging_routes SET session = ?2
                 WHERE id = ?1 AND (session IS NULL OR session = ?2) AND blocked = 0 AND revision = ?3",
                params![route.id, session, route.revision])? == 1, "Messaging route changed while opening its session");
            Ok(())
        })
    }

    pub fn block_route(&self, route: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE messaging_routes SET blocked = 1 WHERE id = ?1",
                [route],
            )?;
            Ok(())
        })
    }

    pub fn restore_route(&self, route: &str) -> Result<bool> {
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE messaging_routes SET blocked = 0, session = NULL, revision = revision + 1 WHERE id = ?1 AND blocked = 1",
                [route],
            )? != 0;
            if changed {
                // A prior admission might already have executed. Freshly staged input may continue.
                tx.execute(
                    "UPDATE messaging_ingress SET stage = 'done', reason = 'previous_session_missing', input = NULL
                     WHERE (route = ?1 OR route IN (SELECT destination FROM messaging_route_bindings WHERE binding = ?1))
                       AND stage IN ('admitting','admitted','uncertain','blocked')",
                    [route],
                )?;
            }
            Ok(changed)
        })
    }

    pub fn inputs(&self, connection: &str) -> Result<Vec<PendingInput>> {
        let db = crate::lock(&self.0);
        let mut query = db.prepare(
            "WITH route_queue AS (
                SELECT COALESCE(link.binding,i.route) AS binding,MIN(i.position) AS head,
                    MIN(CASE WHEN i.stage IN ('pending','admitting') THEN i.position END) AS unadmitted,
                    MIN(CASE WHEN i.stage = 'preparing' THEN i.position END) AS preparing
                FROM messaging_ingress i LEFT JOIN messaging_route_bindings link ON link.destination = i.route
                WHERE i.connection = ?1 AND i.control = 0
                  AND i.stage IN ('preparing','pending','admitting','admitted','uncertain','blocked')
                GROUP BY COALESCE(link.binding,i.route)
             )
             SELECT i.input,i.stage,r.id,r.connection,r.room,r.session,r.blocked,r.revision,
                COALESCE(i.position = q.head,0)
             FROM messaging_ingress i JOIN messaging_routes r ON r.id = i.route
             LEFT JOIN messaging_route_bindings link ON link.destination = i.route
             LEFT JOIN messaging_routes parent ON parent.id = link.binding
             LEFT JOIN route_queue q ON q.binding = COALESCE(link.binding,i.route)
             WHERE i.connection = ?1 AND r.blocked = 0 AND COALESCE(parent.blocked,0) = 0 AND
                 ((i.control = 1 AND i.stage = 'pending') OR
                  (i.control = 0 AND i.stage <> 'preparing' AND
                     (i.position = q.head OR (i.position = q.unadmitted AND
                        (q.preparing IS NULL OR i.position < q.preparing)))))
             ORDER BY i.control DESC,i.position",
        )?;
        Ok(query
            .query_map([connection], |row| {
                Ok(PendingInput {
                    event: decode(row, 0)?,
                    stage: row.get(1)?,
                    route: Route::from_row(row, 2)?,
                    head: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark(
        &self,
        route: &Route,
        event: &str,
        stage: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.write(|tx| set_input(tx, route, event, stage, reason))
    }

    pub fn input(&self, route: &Route, id: &str) -> Result<Option<(String, InboundEvent)>> {
        let db = crate::lock(&self.0);
        Ok(db.query_row(
            "SELECT stage,input FROM messaging_ingress WHERE route = ?1 AND event = ?2 AND control = 0 AND input IS NOT NULL",
            params![route.id, id], |row| Ok((row.get(0)?, decode(row, 1)?)),
        ).optional()?)
    }

    pub fn publish_reply(
        &self,
        route: &Route,
        event: &InboundEvent,
        intent: &DeliveryIntent,
    ) -> Result<()> {
        self.write(|tx| {
            let progress = key("progress", &intent.id)?;
            tx.execute(
                "UPDATE messaging_deliveries SET stage = 'discarded', reason = 'progress_superseded'
                 WHERE id = ?1 AND route = ?2 AND stage = 'pending'",
                params![progress, route.id],
            )?;
            enqueue(tx, route, Some(&event.sender), intent)?;
            set_input(tx, route, &event.id, "done", None)
        })
    }

    pub fn publish_progress(
        &self,
        route: &Route,
        event: &InboundEvent,
        reply_id: String,
    ) -> Result<()> {
        let intent = DeliveryIntent {
            id: key("progress", &reply_id)?,
            room: route.room.clone(),
            reply_anchor: event.reply_anchor.clone(),
            edit_target: None,
            body: DeliveryBody::Progress {
                text: "Working…".into(),
                reply_id,
            },
        };
        self.write(|tx| enqueue(tx, route, Some(&event.sender), &intent))
    }

    pub fn editable_progress(&self, route: &str, reply: &str) -> Result<Option<String>> {
        let progress = key("progress", &reply)?;
        let db = crate::lock(&self.0);
        Ok(db
            .query_row(
                "SELECT p.editable_message FROM messaging_deliveries d
             JOIN messaging_parts p ON p.delivery = d.id
             WHERE d.id = ?1 AND d.route = ?2 AND d.stage = 'sent' AND p.ordinal = 0",
                params![progress, route],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn notification_text(&self, route: &str, id: &str) -> Result<Option<String>> {
        let db = crate::lock(&self.0);
        let intent: Option<DeliveryIntent> = db
            .query_row(
                "SELECT intent FROM messaging_deliveries WHERE route = ?1 AND id = ?2",
                params![route, id],
                |row| decode(row, 0),
            )
            .optional()?;
        Ok(intent.and_then(|intent| match intent.body {
            DeliveryBody::Reply(message) => Some(message.text),
            _ => None,
        }))
    }

    pub fn enqueue(&self, route: &Route, intent: &DeliveryIntent) -> Result<()> {
        self.write(|tx| enqueue(tx, route, None, intent))
    }

    pub fn prompt(
        &self,
        route: &Route,
        event: &InboundEvent,
        action: &Action,
        prompt: Prompt,
    ) -> Result<()> {
        self.write(|tx| {
            if tx.execute(
                "INSERT OR IGNORE INTO messaging_actions (id,connection,route,session,recipient,generation,request,type)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![action.id, route.connection, route.id, action.session, action.recipient,
                    i64::try_from(action.generation)?, action.request, action.kind],
            )? == 0 {
                return Ok(());
            }
            enqueue(tx, route, Some(&event.sender), &DeliveryIntent {
                id: action.id.clone(),
                room: route.room.clone(),
                reply_anchor: event.reply_anchor.clone(),
                edit_target: None,
                body: DeliveryBody::Prompt { action_id: action.id.clone(), prompt },
            })
        })
    }

    pub fn action(&self, route: &Route, id: &str) -> Result<Option<Action>> {
        let db = crate::lock(&self.0);
        Ok(db
            .query_row(
                "SELECT session,recipient,generation,request,type,resolved FROM messaging_actions
             WHERE id = ?1 AND route = ?2 AND connection = ?3",
                params![id, route.id, route.connection],
                |row| {
                    Ok(Action {
                        id: id.into(),
                        route: route.id.clone(),
                        session: row.get(0)?,
                        recipient: row.get(1)?,
                        generation: row.get::<_, i64>(2)?.try_into().unwrap_or_default(),
                        request: row.get(3)?,
                        kind: row.get(4)?,
                        resolved: optional_decode(row, 5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn settle_action(
        &self,
        route: &Route,
        event: &str,
        action: &Action,
        response: &Response,
    ) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE messaging_actions SET resolved = ?2 WHERE id = ?1 AND resolved IS NULL",
                params![action.id, serde_json::to_string(response)?],
            )?;
            set_input(tx, route, event, "done", Some("applied"))
        })
    }

    pub fn ready_deliveries(&self, connection: &str, now: i64) -> Result<Vec<PendingDelivery>> {
        let db = crate::lock(&self.0);
        let mut query = db.prepare(
            "SELECT d.intent,d.route,d.sender,d.stage FROM messaging_deliveries d
             WHERE d.position IN (
                 SELECT MIN(position) FROM messaging_deliveries WHERE connection = ?1
                 AND stage IN ('pending','planned') GROUP BY destination
             )
             AND NOT EXISTS (SELECT 1 FROM messaging_cooldowns c WHERE c.connection = d.connection
                 AND c.until_ms > ?2 AND (c.scope = '' OR c.scope = d.destination))
             AND (d.stage = 'pending' OR EXISTS (
                 SELECT 1 FROM messaging_parts p WHERE p.delivery = d.id AND p.stage = 'pending'
                 AND NOT EXISTS (SELECT 1 FROM messaging_parts preceding WHERE preceding.delivery = p.delivery
                     AND preceding.ordinal < p.ordinal AND preceding.stage NOT IN ('sent','discarded'))
             ))
             ORDER BY d.position",
        )?;
        Ok(query
            .query_map(params![connection, now], |row| {
                Ok(PendingDelivery {
                    intent: decode(row, 0)?,
                    route: row.get(1)?,
                    sender: optional_decode(row, 2)?,
                    planned: row.get::<_, String>(3)? == "planned",
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn plan(&self, connection: &str, id: &str, parts: &[DeliveryPart]) -> Result<()> {
        self.write(|tx| {
            if tx.execute("UPDATE messaging_deliveries SET stage = 'planned' WHERE connection = ?1 AND id = ?2 AND stage = 'pending'",
                params![connection, id])? == 0 {
                return Ok(());
            }
            for (index, part) in parts.iter().enumerate() {
                tx.execute("INSERT INTO messaging_parts (delivery,ordinal,body,stage) VALUES (?1,?2,?3,'pending')",
                    params![id, i64::try_from(index)?, serde_json::to_string(part)?])?;
            }
            Ok(())
        })
    }

    pub fn reject(&self, connection: &str, id: &str, reason: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE messaging_deliveries SET stage = 'rejected',reason = ?3
                        WHERE connection = ?1 AND id = ?2 AND stage IN ('pending','planned')",
                params![connection, id, reason],
            )?;
            Ok(())
        })
    }

    pub fn claim(
        &self,
        connection: &str,
        generation: &str,
        pending: &PendingDelivery,
        now: i64,
    ) -> Result<Option<SendAttempt>> {
        self.write(|tx| {
            let id = &pending.intent.id;
            let part = tx.query_row(
                "UPDATE messaging_parts SET stage = 'sending',attempt = attempt + 1,generation = ?3,reason = NULL
                 WHERE delivery = ?2 AND stage = 'pending' AND ordinal = (
                     SELECT MIN(ordinal) FROM messaging_parts WHERE delivery = ?2 AND stage NOT IN ('sent','discarded')
                 ) AND EXISTS (
                     SELECT 1 FROM messaging_deliveries d WHERE d.id = ?2 AND d.connection = ?1 AND d.stage = 'planned'
                     AND NOT EXISTS (SELECT 1 FROM messaging_cooldowns c WHERE c.connection = d.connection
                         AND c.until_ms > ?4 AND (c.scope = '' OR c.scope = d.destination))
                 )
                 RETURNING ordinal,attempt,body",
                params![connection, id, generation, now], |row| Ok(SendAttempt {
                    delivery_id: id.clone(), part: row.get(0)?, attempt: row.get(1)?,
                    reply_anchor: pending.intent.reply_anchor.clone(), payload: decode(row, 2)?,
                }),
            ).optional()?;
            Ok(part)
        })
    }

    pub fn release(&self, connection: &str, generation: &str, attempt: &SendAttempt) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE messaging_parts SET stage = 'pending',reason = 'send_cancelled_before_dispatch',generation = NULL
                 WHERE delivery = ?2 AND ordinal = ?3 AND attempt = ?4 AND generation = ?5 AND stage = 'sending'
                 AND EXISTS(SELECT 1 FROM messaging_deliveries WHERE id = ?2 AND connection = ?1)",
                params![connection, attempt.delivery_id, attempt.part, attempt.attempt, generation],
            )?;
            Ok(())
        })
    }

    pub fn record(
        &self,
        connection: &str,
        generation: Option<&str>,
        id: &str,
        (part, attempt): (u32, u32),
        result: DeliveryResult,
        now: i64,
    ) -> Result<bool> {
        let (stage, reason, receipt, editable_message, retry) = match result {
            DeliveryResult::Confirmed {
                receipt,
                editable_message,
            } if super::runtime::valid_id(&receipt) => (
                "sent",
                None,
                Some(receipt),
                editable_message.filter(|id| super::runtime::valid_id(id)),
                None,
            ),
            DeliveryResult::Retryable { after, scope } => (
                "pending",
                Some("rate_limited"),
                None,
                None,
                Some((after, scope)),
            ),
            DeliveryResult::Rejected => ("rejected", Some("platform_rejected"), None, None, None),
            _ => ("uncertain", Some("send_outcome_unknown"), None, None, None),
        };
        self.write(|tx| {
            if tx.execute(
                "UPDATE messaging_parts SET stage = ?6,receipt = ?7,reason = ?8,editable_message = ?9,generation = NULL
                 WHERE delivery = ?2 AND ordinal = ?3 AND attempt = ?4 AND generation IS ?5
                 AND stage = CASE WHEN ?5 IS NULL THEN 'uncertain' ELSE 'sending' END
                 AND EXISTS(SELECT 1 FROM messaging_deliveries WHERE id = ?2 AND connection = ?1)",
                params![connection, id, part, attempt, generation, stage, receipt, reason, editable_message],
            )? == 0 {
                return Ok(false);
            }
            if let Some((after, scope)) = retry {
                let millis = i64::try_from(after.as_millis()).unwrap_or(i64::MAX).max(1);
                tx.execute(
                    "INSERT INTO messaging_cooldowns (connection,scope,until_ms)
                     SELECT connection,CASE WHEN ?2 THEN destination ELSE '' END,?3 FROM messaging_deliveries WHERE id = ?1
                     ON CONFLICT(connection,scope) DO UPDATE SET until_ms = MAX(until_ms,excluded.until_ms)",
                    params![id, scope == RetryScope::Destination, now.saturating_add(millis)],
                )?;
            }
            finish_delivery(tx, id)?;
            Ok(true)
        })
    }

    pub fn uncertain_deliveries(&self, connection: &str) -> Result<Vec<UncertainDelivery>> {
        let db = crate::lock(&self.0);
        let mut query = db.prepare(
            "SELECT d.id,p.ordinal,p.attempt,r.room FROM messaging_deliveries d
             JOIN messaging_parts p ON p.delivery = d.id JOIN messaging_routes r ON r.id = d.route
             WHERE d.connection = ?1 AND p.stage = 'uncertain' ORDER BY d.position LIMIT 200",
        )?;
        Ok(query
            .query_map([connection], |row| {
                Ok(UncertainDelivery {
                    delivery_id: row.get(0)?,
                    part: row.get(1)?,
                    attempt: row.get(2)?,
                    room: decode(row, 3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn recover_sends(&self, connection: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE messaging_parts SET stage = 'uncertain',reason = 'send_interrupted',generation = NULL
                 WHERE stage = 'sending' AND delivery IN (SELECT id FROM messaging_deliveries WHERE connection = ?1)",
                [connection],
            )?;
            Ok(())
        })
    }

    pub fn resolve(
        &self,
        connection: &str,
        delivery: &str,
        part: u32,
        attempt: u32,
        resolution: DeliveryResolution,
    ) -> Result<bool> {
        let (stage, reason) = match resolution {
            DeliveryResolution::Delivered => ("sent", "operator_confirmed"),
            DeliveryResolution::Discard => ("discarded", "operator_discarded"),
        };
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE messaging_parts SET stage = ?5,reason = ?6
                 WHERE delivery = ?2 AND ordinal = ?3 AND attempt = ?4 AND stage = 'uncertain'
                 AND EXISTS(SELECT 1 FROM messaging_deliveries WHERE id = ?2 AND connection = ?1)",
                params![connection, delivery, part, attempt, stage, reason],
            )? != 0;
            if changed {
                finish_delivery(tx, delivery)?;
            }
            Ok(changed)
        })
    }

    pub fn delivery_status(&self) -> Result<Vec<DeliveryStatus>> {
        let db = crate::lock(&self.0);
        let mut query = db.prepare(
            "SELECT d.id,d.connection,d.route,d.stage,p.ordinal,p.attempt,p.stage,COALESCE(p.reason,d.reason)
             FROM messaging_deliveries d LEFT JOIN messaging_parts p ON p.delivery = d.id
             ORDER BY (p.stage = 'uncertain') DESC,d.position DESC,p.ordinal LIMIT 200",
        )?;
        Ok(query
            .query_map([], |row| {
                Ok(DeliveryStatus {
                    id: row.get(0)?,
                    connection: row.get(1)?,
                    route: row.get(2)?,
                    state: row.get(3)?,
                    part: row.get(4)?,
                    attempt: row.get(5)?,
                    part_state: row.get(6)?,
                    error: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn input_status(&self) -> Result<Vec<InputStatus>> {
        let db = crate::lock(&self.0);
        let mut query = db.prepare(
            "SELECT i.event,i.connection,r.id,COALESCE(parent.session,r.session),
                CASE WHEN (r.blocked OR parent.blocked) AND i.stage NOT IN ('done','refused') THEN 'blocked' ELSE i.stage END AS state,
                COALESCE(i.reason, CASE WHEN (r.blocked OR parent.blocked) AND i.stage NOT IN ('done','refused') THEN 'route_unavailable' END),
                i.conflict
             FROM messaging_ingress i LEFT JOIN messaging_routes r ON i.route = r.id
             LEFT JOIN messaging_route_bindings link ON link.destination = r.id
             LEFT JOIN messaging_routes parent ON parent.id = link.binding
             ORDER BY (state IN ('blocked','uncertain')) DESC,i.position DESC LIMIT 200",
        )?;
        Ok(query
            .query_map([], |row| {
                Ok(InputStatus {
                    id: row.get(0)?,
                    connection: row.get(1)?,
                    route: row.get(2)?,
                    session: row.get(3)?,
                    state: row.get(4)?,
                    reason: row.get(5)?,
                    conflict: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

pub(super) fn read_route(db: &Connection, id: &str) -> rusqlite::Result<Option<Route>> {
    db.query_row(
        "SELECT id,connection,room,session,blocked,revision FROM messaging_routes WHERE id = ?1",
        [id],
        |row| Route::from_row(row, 0),
    )
    .optional()
}

pub(super) fn read_checkpoint(
    db: &Connection,
    connection: &str,
    stream: &str,
) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT value FROM messaging_checkpoints WHERE connection = ?1 AND stream = ?2",
        params![connection, stream],
        |row| row.get(0),
    )
    .optional()
}

pub(super) fn check_advance(
    db: &Connection,
    connection: &str,
    advance: Option<&Advance>,
) -> Result<bool> {
    Ok(match advance {
        Some(a) => read_checkpoint(db, connection, &a.stream)? == a.previous,
        None => true,
    })
}

pub(super) fn write_advance(
    db: &Connection,
    connection: &str,
    advance: Option<&Advance>,
) -> Result<Option<String>> {
    let Some(a) = advance else { return Ok(None); };
    db.execute(
        "INSERT INTO messaging_checkpoints (connection,stream,value) VALUES (?1,?2,?3)
         ON CONFLICT(connection,stream) DO UPDATE SET value = excluded.value",
        params![connection, a.stream, a.checkpoint],
    )?;
    Ok(Some(a.checkpoint.clone()))
}

pub(super) fn intake_receipt(
    db: &Connection,
    connection: &str,
    event: &str,
) -> Result<Option<IntakeReceipt>> {
    Ok(db.query_row(
        "SELECT route,disposition FROM messaging_ingress WHERE connection = ?1 AND event = ?2",
        params![connection,event],
        |row| Ok(IntakeReceipt { id: event.to_owned(), route: row.get(0)?,
            disposition: decode(row, 1)?, repeated: false }),
    ).optional()?)
}

pub(super) fn fingerprint(event: &InboundEvent) -> Result<String> {
    // Roles and mention evidence can change across a redelivery.
    key("event", &(&event.room, &event.sender.id, &event.reply_anchor, &event.body))
}

pub(super) fn append(
    tx: &Transaction<'_>,
    connection: &str,
    revision: i64,
    candidate: Candidate,
    preparing: bool,
) -> Result<IntakeReceipt> {
    let Candidate { event, decision } = candidate;
    let identity = fingerprint(&event)?;
    let existing = tx.query_row(
        "SELECT fingerprint,route,disposition FROM messaging_ingress WHERE connection = ?1 AND event = ?2",
        params![connection, event.id],
        |row| Ok((row.get::<_, String>(0)?, row.get(1)?, decode(row, 2)?)),
    ).optional()?;
    if let Some((old, route, disposition)) = existing {
        if old != identity {
            tx.execute(
                "UPDATE messaging_ingress SET conflict = 1 WHERE connection = ?1 AND event = ?2",
                params![connection, event.id],
            )?;
            return Ok(IntakeReceipt {
                id: event.id,
                route: None,
                disposition: Disposition::Conflict,
                repeated: false,
            });
        }
        return Ok(IntakeReceipt {
            id: event.id,
            route,
            disposition,
            repeated: true,
        });
    }
    let decision = if preparing {
        decision
    } else {
        attachment_decision(tx, connection, &event, decision)?
    };
    let (route, disposition, input, stage) = match decision {
        Ok(route) => {
            tx.execute(
                "INSERT OR IGNORE INTO messaging_routes (id,connection,room) VALUES (?1,?2,?3)",
                params![route.id, connection, serde_json::to_string(&route.room)?],
            )?;
            (
                Some(route.id),
                if preparing { Disposition::Deferred } else { Disposition::Accepted },
                Some(serde_json::to_string(&event)?),
                if preparing { "preparing" } else { "pending" },
            )
        }
        Err(disposition) => (None, disposition, None, "refused"),
    };
    tx.execute(
        "INSERT INTO messaging_ingress (connection,event,fingerprint,route,disposition,input,control,policy_revision,stage)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![connection, event.id, identity, route, serde_json::to_string(&disposition)?,
            input, event.body.is_control(), revision, stage],
    )?;
    Ok(IntakeReceipt {
        id: event.id,
        route,
        disposition,
        repeated: false,
    })
}

pub(super) fn attachment_decision(
    tx: &Transaction<'_>,
    connection: &str,
    event: &InboundEvent,
    decision: Result<Route, Disposition>,
) -> Result<Result<Route, Disposition>> {
    let Ok(route) = decision else { return Ok(decision) };
    if let InboundBody::Message(message) = &event.body
        && !message.attachments.is_empty()
    {
        let mut exists = tx.prepare(
            "SELECT EXISTS(SELECT 1 FROM messaging_media WHERE connection = ?1 AND id = ?2)",
        )?;
        for attachment in &message.attachments {
            if !exists.query_row(params![connection, attachment.id], |r| r.get::<_, bool>(0))? {
                return Ok(Err(Disposition::Invalid));
            }
        }
    }
    Ok(Ok(route))
}

pub(super) fn set_input(
    db: &Connection,
    route: &Route,
    event: &str,
    stage: &str,
    reason: Option<&str>,
) -> Result<()> {
    db.execute(
        "UPDATE messaging_ingress SET stage = ?3,reason = ?4,input = CASE WHEN ?3 = 'done' THEN NULL ELSE input END
         WHERE route = ?1 AND event = ?2 AND stage NOT IN ('done','refused')",
        params![route.id, event, stage, reason],
    )?;
    Ok(())
}

fn enqueue(
    db: &Connection,
    route: &Route,
    sender: Option<&Sender>,
    intent: &DeliveryIntent,
) -> Result<()> {
    ensure!(
        route.room == intent.room,
        "Messaging destination does not match the route"
    );
    let body = serde_json::to_string(intent)?;
    let destination = key("destination", &(&route.room.chat, &route.room.thread))?;
    let inserted = db.execute(
        "INSERT OR IGNORE INTO messaging_deliveries (id,connection,destination,route,intent,sender,stage)
         VALUES (?1,?2,?3,?4,?5,?6,'pending')",
        params![intent.id, route.connection, destination, route.id, body, sender.map(serde_json::to_string).transpose()?],
    )?;
    if inserted == 0 {
        let same: bool = db.query_row(
            "SELECT route = ?2 AND intent = ?3 FROM messaging_deliveries WHERE id = ?1",
            params![intent.id, route.id, body],
            |row| row.get(0),
        )?;
        ensure!(
            same,
            "Messaging delivery identity reused for a different intent"
        );
    }
    Ok(())
}

fn finish_delivery(db: &Connection, id: &str) -> Result<()> {
    db.execute(
        "UPDATE messaging_deliveries SET stage = CASE
            WHEN EXISTS (SELECT 1 FROM messaging_parts WHERE delivery = ?1 AND stage = 'rejected') THEN 'rejected'
            WHEN EXISTS (SELECT 1 FROM messaging_parts WHERE delivery = ?1 AND stage NOT IN ('sent','discarded')) THEN 'planned'
            WHEN EXISTS (SELECT 1 FROM messaging_parts WHERE delivery = ?1 AND stage = 'discarded') THEN 'discarded'
            ELSE 'sent' END
         WHERE id = ?1 AND stage = 'planned'",
        [id],
    )?;
    Ok(())
}
