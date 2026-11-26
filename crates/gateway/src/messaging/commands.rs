use super::{
    runtime::{Ingress, blocking, db, valid_id},
    store::{self, PendingInput, Route, Store},
    types::*,
};
use crate::{LiveSession, lock};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_native_invocations (
    connection TEXT NOT NULL, event TEXT NOT NULL, route TEXT NOT NULL, actor TEXT NOT NULL,
    outcome TEXT NOT NULL, PRIMARY KEY(connection,event)
);
CREATE TABLE IF NOT EXISTS messaging_route_models (
    route TEXT PRIMARY KEY REFERENCES messaging_routes(id), model TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS messaging_home_routes (
    connection TEXT PRIMARY KEY, route TEXT NOT NULL REFERENCES messaging_routes(id)
);
CREATE TABLE IF NOT EXISTS messaging_fetch_actions (
    id TEXT PRIMARY KEY, connection TEXT NOT NULL, route TEXT NOT NULL, actor TEXT NOT NULL,
    input TEXT NOT NULL, delivery TEXT NOT NULL UNIQUE, expires INTEGER NOT NULL,
    claimed_by TEXT, released INTEGER NOT NULL DEFAULT 0
);
";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Status,
    Queue,
    SelectModel { model: String },
    UseSkill { skill: String, input: String },
    SetHome,
    FetchResult { action: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CommandDescriptor {
    pub name: &'static str,
    pub arguments: &'static [&'static str],
    pub operator: bool,
}

pub(super) const COMMANDS: [CommandDescriptor; 6] = [
    CommandDescriptor {
        name: "status",
        arguments: &[],
        operator: false,
    },
    CommandDescriptor {
        name: "queue",
        arguments: &[],
        operator: false,
    },
    CommandDescriptor {
        name: "model",
        arguments: &["model"],
        operator: true,
    },
    CommandDescriptor {
        name: "skill",
        arguments: &["skill", "input"],
        operator: false,
    },
    CommandDescriptor {
        name: "set-home",
        arguments: &[],
        operator: true,
    },
    CommandDescriptor {
        name: "fetch-result",
        arguments: &["action"],
        operator: false,
    },
];

impl Command {
    pub(super) fn operator(&self) -> bool {
        matches!(self, Self::SelectModel { .. } | Self::SetHome)
    }

    pub(super) fn valid(&self) -> bool {
        match self {
            Self::SelectModel { model } => {
                valid_id(model) && !model.chars().any(char::is_whitespace)
            }
            Self::UseSkill { skill, input } => {
                valid_id(skill)
                    && input.len() <= super::runtime::MAX_TEXT
                    && !input.trim().is_empty()
            }
            Self::FetchResult { action } => valid_id(action),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOutcome {
    Ready,
    Queue { pending: usize, running: bool },
    Applied,
    PendingResult,
    ResultReady { delivery: String },
    AlreadyClaimed,
    Expired,
    Forbidden,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FetchAction {
    pub id: String,
    pub expires_ms: i64,
}

impl Ingress {
    pub fn commands(
        &self,
        room: Room,
        sender: Sender,
    ) -> Result<Vec<CommandDescriptor>, GatewayError> {
        self.checked(true, |state| {
            let event = InboundEvent {
                id: "command-discovery".into(),
                room,
                sender,
                reply_anchor: None,
                mentioned: true,
                body: InboundBody::Command(Command::Status),
            };
            if self.route_event(&state.policy, &event)?.is_err() {
                return Err(GatewayError::Forbidden);
            }
            let operator = state.policy.operator(&event.sender);
            Ok(COMMANDS
                .iter()
                .copied()
                .filter(|item| !item.operator || operator)
                .collect())
        })
    }

    /// Read the durable invocation receipt for the original actor and room.
    pub async fn invocation_outcome(
        &self,
        event_id: String,
        room: Room,
        sender: Sender,
    ) -> Result<Option<InvocationOutcome>, GatewayError> {
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |state| {
                let event = InboundEvent {
                    id: event_id,
                    room,
                    sender,
                    reply_anchor: None,
                    mentioned: true,
                    body: InboundBody::Command(Command::Status),
                };
                let Ok(route) = scope.route_event(&state.policy, &event)? else {
                    return Err(GatewayError::Forbidden);
                };
                db(scope.store.invocation(&route, &event.id, &event.sender.id))
            })
        })
        .await
    }

    /// Reserve a pending-result action before publishing its button or other
    /// callback. It uses the original logical input and existing outbox reply.
    pub async fn defer_result(
        &self,
        input: String,
        room: Room,
        sender: Sender,
        expires_ms: i64,
    ) -> Result<FetchAction, GatewayError> {
        if !valid_id(&input)
            || expires_ms <= chrono::Utc::now().timestamp_millis()
            || !self.capabilities().interactive_prompts
        {
            return Err(GatewayError::Invalid);
        }
        let scope = self.clone();
        blocking(move || {
            scope.checked(true, |state| {
                let event = InboundEvent {
                    id: input,
                    room,
                    sender,
                    reply_anchor: None,
                    mentioned: true,
                    body: InboundBody::Command(Command::Status),
                };
                let Ok(route) = scope.route_event(&state.policy, &event)? else {
                    return Err(GatewayError::Forbidden);
                };
                db(scope
                    .store
                    .defer_result(&route, &event.id, &event.sender.id, expires_ms))?
                .ok_or(GatewayError::Forbidden)
            })
        })
        .await
    }

    /// Release exactly the final claimed by this invocation, after the
    /// adapter has bound any provider reply grant to that effect. The current
    /// actor and room still have to be eligible.
    pub async fn release_result(
        &self,
        action: String,
        invocation: String,
        room: Room,
        sender: Sender,
    ) -> Result<bool, GatewayError> {
        let scope = self.clone();
        let changed = blocking(move || {
            scope.checked(true, |state| {
                let event = InboundEvent {
                    id: invocation,
                    room,
                    sender,
                    reply_anchor: None,
                    mentioned: true,
                    body: InboundBody::Command(Command::FetchResult {
                        action: action.clone(),
                    }),
                };
                let Ok(route) = scope.route_event(&state.policy, &event)? else {
                    return Err(GatewayError::Forbidden);
                };
                db(scope.store.release_result(
                    &route,
                    &action,
                    &event.id,
                    &event.sender.id,
                    chrono::Utc::now().timestamp_millis(),
                ))
            })
        })
        .await?;
        if changed {
            self.notify();
        }
        Ok(changed)
    }

    pub fn home_destination(&self) -> Result<Option<String>, GatewayError> {
        self.checked(true, |_| db(self.store.home(&self.connection)))
    }
}

pub(super) fn execute(
    scope: &Ingress,
    work: &PendingInput,
    command: &Command,
    session: Option<&LiveSession>,
    allowed: bool,
    operator: bool,
) -> Result<(), GatewayError> {
    let operation = if !allowed || command.operator() && !operator {
        Operation::Output(InvocationOutcome::Forbidden)
    } else {
        match command {
            Command::Status => Operation::Output(InvocationOutcome::Ready),
            Command::Queue => {
                let queue = session
                    .map(|session| {
                        let view = lock(&session.view);
                        InvocationOutcome::Queue {
                            pending: view
                                .authority
                                .receipts
                                .iter()
                                .filter(|r| r.status == "queued")
                                .count(),
                            running: view.running,
                        }
                    })
                    .unwrap_or(InvocationOutcome::Queue {
                        pending: 0,
                        running: false,
                    });
                Operation::Output(queue)
            }
            Command::SelectModel { model } => Operation::SelectModel(model),
            Command::SetHome => Operation::Home,
            Command::FetchResult { action } => Operation::Fetch(action),
            Command::UseSkill { .. } => Operation::Output(InvocationOutcome::Unsupported),
        }
    };
    db(scope
        .store
        .complete_command(&work.route, &work.event, operation))
}

enum Operation<'a> {
    Output(InvocationOutcome),
    SelectModel(&'a str),
    Home,
    Fetch(&'a str),
}

impl Store {
    pub(super) fn route_model(&self, route: &str) -> Result<Option<String>> {
        self.read(|db| {
            Ok(db
                .query_row(
                    "SELECT model FROM messaging_route_models WHERE route = ?1",
                    [route],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    fn home(&self, connection: &str) -> Result<Option<String>> {
        self.read(|db| {
            Ok(db
                .query_row(
                    "SELECT route FROM messaging_home_routes WHERE connection = ?1",
                    [connection],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    fn invocation(
        &self,
        route: &Route,
        event: &str,
        actor: &str,
    ) -> Result<Option<InvocationOutcome>> {
        self.read(|db| {
            Ok(db.query_row(
            "SELECT outcome FROM messaging_native_invocations WHERE connection = ?1 AND event = ?2
              AND route = ?3 AND actor = ?4",
            params![route.connection,event,route.id,actor], |r| store::decode(r,0),
        ).optional()?)
        })
    }

    fn complete_command(
        &self,
        route: &Route,
        event: &InboundEvent,
        operation: Operation<'_>,
    ) -> Result<()> {
        self.write(|tx| {
            let existing = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM messaging_native_invocations WHERE connection = ?1 AND event = ?2)",
                params![route.connection,event.id], |row| row.get::<_,bool>(0),
            )?;
            if existing { return Ok(()) }
            let outcome = match operation {
                Operation::Output(outcome) => outcome,
                Operation::SelectModel(model) => {
                    tx.execute("INSERT INTO messaging_route_models(route,model) VALUES (?1,?2)
                                ON CONFLICT(route) DO UPDATE SET model = excluded.model", params![route.id,model])?;
                    InvocationOutcome::Applied
                }
                Operation::Home => {
                    if tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM messaging_verified_destinations WHERE route = ?1 AND expires > ?2)",
                        params![route.id, chrono::Utc::now().timestamp_millis()], |r| r.get::<_,bool>(0),
                    )? {
                        tx.execute("INSERT INTO messaging_home_routes(connection,route) VALUES (?1,?2)
                                  ON CONFLICT(connection) DO UPDATE SET route = excluded.route", params![route.connection,route.id])?;
                        InvocationOutcome::Applied
                    } else { InvocationOutcome::Forbidden }
                }
                Operation::Fetch(action) => claim_result(tx, route, event, action)?,
            };
            tx.execute(
                "INSERT INTO messaging_native_invocations(connection,event,route,actor,outcome) VALUES (?1,?2,?3,?4,?5)",
                params![route.connection,event.id,route.id,event.sender.id,serde_json::to_string(&outcome)?],
            )?;
            store::set_input(tx, route, &event.id, "done", Some("command_complete"))
        })
    }

    fn defer_result(
        &self,
        route: &Route,
        input: &str,
        actor: &str,
        expires: i64,
    ) -> Result<Option<FetchAction>> {
        let id = store::key(
            "fetch-action",
            &(&route.connection, input, &route.id, actor),
        )?;
        let delivery = store::key(
            "reply",
            &store::key("channel-input", &(&route.connection, input))?,
        )?;
        self.write(|tx| {
            if let Some(expires_ms) = tx.query_row(
                "SELECT expires FROM messaging_fetch_actions WHERE id = ?1", [&id], |r| r.get::<_, i64>(0),
            ).optional()? {
                return Ok(Some(FetchAction { id, expires_ms }));
            }
            let source = tx.query_row(
                "SELECT input FROM messaging_ingress WHERE connection = ?1 AND event = ?2 AND route = ?3
                   AND stage IN ('pending','admitted') AND control = 0",
                params![route.connection,input,route.id], |r| store::decode::<InboundEvent>(r,0),
            ).optional()?;
            if source.is_none_or(|source| source.sender.id != actor) {
                return Ok(None);
            }
            ensure!(tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM messaging_deliveries WHERE id = ?1)",
                [&delivery], |r| r.get::<_,bool>(0))?, "The final was already published to the outbox");
            tx.execute(
                "INSERT INTO messaging_fetch_actions(id,connection,route,actor,input,delivery,expires)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)", params![id,route.connection,route.id,actor,input,delivery,expires],
            )?;
            Ok(Some(FetchAction { id, expires_ms: expires }))
        })
    }

    fn release_result(
        &self,
        route: &Route,
        action: &str,
        invocation: &str,
        actor: &str,
        now: i64,
    ) -> Result<bool> {
        self.write(|tx| Ok(tx.execute(
            "UPDATE messaging_fetch_actions SET released = 1 WHERE id = ?1 AND connection = ?2 AND route = ?3
                AND actor = ?4 AND claimed_by = ?5 AND expires > ?6 AND released = 0",
            params![action,route.connection,route.id,actor,invocation,now],
        )? == 1))
    }

    pub(super) fn result_released(&self, connection: &str, delivery: &str) -> Result<bool> {
        self.read(|db| Ok(db.query_row(
            "SELECT COALESCE((SELECT released FROM messaging_fetch_actions WHERE connection = ?1 AND delivery = ?2),1)",
            params![connection,delivery], |r| r.get(0),
        )?))
    }
}

fn claim_result(
    db: &rusqlite::Connection,
    route: &Route,
    event: &InboundEvent,
    id: &str,
) -> Result<InvocationOutcome> {
    let item = db.query_row(
        "SELECT f.delivery,f.expires,f.claimed_by,
                EXISTS(SELECT 1 FROM messaging_deliveries WHERE id = f.delivery)
         FROM messaging_fetch_actions f WHERE f.id = ?1 AND f.connection = ?2 AND f.route = ?3 AND f.actor = ?4",
        params![id,route.connection,route.id,event.sender.id],
        |r| Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,bool>(3)?)),
    ).optional()?;
    let Some((delivery, expires, claimed, ready)) = item else {
        return Ok(InvocationOutcome::Forbidden);
    };
    if expires <= chrono::Utc::now().timestamp_millis() {
        return Ok(InvocationOutcome::Expired);
    }
    if claimed.is_some() {
        return Ok(InvocationOutcome::AlreadyClaimed);
    }
    if !ready {
        return Ok(InvocationOutcome::PendingResult);
    }
    db.execute(
        "UPDATE messaging_fetch_actions SET claimed_by = ?2 WHERE id = ?1",
        params![id, event.id],
    )?;
    Ok(InvocationOutcome::ResultReady { delivery })
}
