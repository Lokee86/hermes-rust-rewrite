use super::{
    Advance, Disposition, GatewayError, InboundBody, InboundEvent, Ingress, IntakeReceipt,
    IntakeResult,
    repair::blocks_intake,
    runtime::{blocking, db, valid_advance, valid_id, valid_input_id},
    store::{self, Candidate, Store},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::BTreeSet;

const EVENT_NAMESPACE: &str = "event";

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_input_aliases (
    connection TEXT NOT NULL, namespace TEXT NOT NULL, provider_id TEXT NOT NULL,
    event TEXT NOT NULL, PRIMARY KEY(connection, namespace, provider_id),
    FOREIGN KEY(connection,event) REFERENCES messaging_ingress(connection,event)
);
CREATE TABLE IF NOT EXISTS messaging_input_alias_conflicts (
    connection TEXT NOT NULL, observation TEXT NOT NULL,
    PRIMARY KEY(connection, observation)
);
";

/// One authenticated identifier for the same provider input. The adapter
/// proves the association before submission. Display names or text are not IDs.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct InputAlias {
    pub namespace: String,
    pub id: String,
}

impl Ingress {
    pub async fn submit_aliased(
        &self,
        event: InboundEvent,
        mut aliases: Vec<InputAlias>,
        advance: Option<Advance>,
    ) -> Result<IntakeResult, GatewayError> {
        if !valid_input_id(&event.id)
            || !matches!(event.body, InboundBody::Message(_))
            || !(1..=32).contains(&aliases.len())
            || !valid_advance(advance.as_ref())
            || aliases.iter().any(|alias| {
                !valid_id(&alias.namespace)
                    || !valid_id(&alias.id)
                    || alias.namespace == EVENT_NAMESPACE && !valid_input_id(&alias.id)
            })
        {
            return Err(GatewayError::Invalid);
        }
        aliases.push(InputAlias {
            namespace: EVENT_NAMESPACE.into(),
            id: event.id.clone(),
        });
        aliases.sort_unstable();
        aliases.dedup();
        let scope = self.clone();
        let result = blocking(move || {
            scope.checked(true, |state| {
                let decision = scope.route_event(&state.policy, &event)?;
                let candidate = scope.intake_candidate(&state.policy, event, decision, None)?;
                let advance = scope.scoped_advance(advance)?;
                db(scope.store.intake_aliases(
                    &scope.connection,
                    state.revision,
                    candidate,
                    &aliases,
                    advance.as_ref(),
                ))?
            })
        })
        .await?;
        self.notify();
        Ok(result)
    }
}

fn alias_target(
    db: &Connection,
    connection: &str,
    namespace: &str,
    id: &str,
) -> Result<Option<String>> {
    Ok(db.query_row(
        "SELECT event FROM messaging_input_aliases WHERE connection=?1 AND namespace=?2 AND provider_id=?3",
        params![connection,namespace,id],
        |row| row.get(0),
    ).optional()?)
}

pub(super) fn event_target(db: &Connection, connection: &str, id: &str) -> Result<Option<String>> {
    alias_target(db, connection, EVENT_NAMESPACE, id)
}

impl Store {
    fn intake_aliases(
        &self,
        connection: &str,
        revision: i64,
        mut candidate: Candidate,
        aliases: &[InputAlias],
        advance: Option<&Advance>,
    ) -> Result<Result<IntakeResult, GatewayError>> {
        self.write(|tx| {
            if !store::check_advance(tx, connection, advance)?
                || blocks_intake(tx, connection, &candidate.event)?
            {
                return Ok(Err(GatewayError::CheckpointChanged));
            }
            let mut targets = BTreeSet::new();
            for alias in aliases {
                if let Some(id) = alias_target(tx, connection, &alias.namespace, &alias.id)? {
                    targets.insert(id);
                } else if alias.namespace == EVENT_NAMESPACE
                    && store::intake_receipt(tx, connection, &alias.id)?.is_some()
                {
                    targets.insert(alias.id.clone());
                }
            }
            let receipt = if targets.len() > 1 {
                let fingerprint = store::fingerprint(&candidate.event)?;
                let observation = store::key("alias-conflict", &(&candidate.event.id, fingerprint, aliases))?;
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO messaging_input_alias_conflicts(connection,observation) VALUES (?1,?2)",
                    params![connection,observation],
                )?;
                IntakeReceipt { id: candidate.event.id, route: None, disposition: Disposition::Conflict, repeated: inserted == 0 }
            } else {
                if let Some(id) = targets.pop_first() {
                    candidate.event.id = id;
                }
                let authorized = candidate.decision.as_ref().ok().map(|route| route.id.clone());
                let receipt = store::append(tx, connection, revision, candidate, false)?;
                if authorized.is_some() && authorized == receipt.route
                    && matches!(receipt.disposition, Disposition::Accepted | Disposition::Deferred | Disposition::Coalesced)
                {
                    for alias in aliases {
                        tx.execute(
                            "INSERT OR IGNORE INTO messaging_input_aliases(connection,namespace,provider_id,event)
                             VALUES (?1,?2,?3,?4)",
                            params![connection, alias.namespace, alias.id, receipt.id],
                        )?;
                    }
                }
                receipt
            };
            let checkpoint = store::write_advance(tx, connection, advance)?;
            Ok(Ok(IntakeResult { receipts: vec![receipt], checkpoint }))
        })
    }
}
