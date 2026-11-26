use super::{
    Effect, EffectOperation, EffectReceipt, ReceiptKind,
    profile::VerifiedInstallation,
    routes,
    runtime::{now, valid_id},
    store::{
        Route, Store, decode, eligible_consent, optional_decode, protection_matches, read_route,
    },
    types::{AccessPolicy, DeliveryPart, Room, Sender},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_provision_attempts (
    delivery TEXT NOT NULL, ordinal INTEGER NOT NULL, attempt INTEGER NOT NULL, scope TEXT NOT NULL,
    PRIMARY KEY(delivery,ordinal,attempt),
    FOREIGN KEY(delivery,ordinal,attempt) REFERENCES messaging_effect_attempts(delivery,ordinal,attempt)
);
";

#[derive(Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct ProvisionScope {
    parent: String,
    parent_revision: i64,
    binding: String,
    binding_revision: i64,
    installation: Option<VerifiedInstallation>,
}

impl ProvisionScope {
    pub(super) fn capture(
        db: &Connection,
        route: &str,
        connection: &str,
        room: &Room,
        installation: Option<&VerifiedInstallation>,
    ) -> Result<Option<Self>> {
        let Some(parent) = read_route(db, route)? else {
            return Ok(None);
        };
        let binding = routes::conversation(db, &parent)?;
        Ok(
            (parent.room == *room && valid_routes(connection, &parent, &binding)).then(|| Self {
                parent: parent.id,
                parent_revision: parent.revision,
                binding: binding.id,
                binding_revision: binding.revision,
                installation: installation.cloned(),
            }),
        )
    }

    pub(super) fn for_attempt(
        db: &Connection,
        delivery: &str,
        part: u32,
        attempt: u32,
    ) -> Result<Option<Self>> {
        Ok(db.query_row(
            "SELECT scope FROM messaging_provision_attempts WHERE delivery=?1 AND ordinal=?2 AND attempt=?3",
            params![delivery, part, attempt], |row| decode(row, 0),
        ).optional()?)
    }

    pub(super) fn record(
        &self,
        tx: &Transaction<'_>,
        delivery: &str,
        part: u32,
        attempt: u32,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO messaging_provision_attempts(delivery,ordinal,attempt,scope) VALUES (?1,?2,?3,?4)
             ON CONFLICT(delivery,ordinal,attempt) DO NOTHING",
            params![delivery, part, attempt, serde_json::to_string(self)?],
        )?;
        Ok(())
    }

    pub(super) fn allows_installation(&self, installation: Option<&VerifiedInstallation>) -> bool {
        self.installation.as_ref() == installation
    }
}

fn valid_routes(connection: &str, parent: &Route, binding: &Route) -> bool {
    parent.connection == connection
        && binding.connection == connection
        && !parent.blocked
        && !binding.blocked
}

#[derive(Eq, PartialEq)]
pub(super) struct ProvisionedDestination {
    pub scope: ProvisionScope,
    pub parent: Route,
    pub binding: Route,
    pub room: Room,
    pub sender: Option<Sender>,
    pub effect: Effect,
}

fn provisioned_destination(
    db: &Connection,
    connection: &str,
    delivery: &str,
    (part, attempt): (u32, u32),
) -> Result<Option<ProvisionedDestination>> {
    let evidence = db.query_row(
        "SELECT d.route, d.sender, p.body, ep.receipt, q.scope FROM messaging_deliveries d
         JOIN messaging_parts p ON p.delivery=d.id
         JOIN messaging_effect_parts ep ON ep.delivery=p.delivery AND ep.ordinal=p.ordinal
         JOIN messaging_effect_attempts a ON a.delivery=p.delivery AND a.ordinal=p.ordinal AND a.attempt=p.attempt
         JOIN messaging_provision_attempts q ON q.delivery=a.delivery AND q.ordinal=a.ordinal AND q.attempt=a.attempt
         WHERE d.connection=?1 AND d.id=?2 AND p.ordinal=?3 AND p.attempt=?4 AND p.stage='sent'
           AND a.base_request=p.body AND json_extract(ep.receipt,'$.id')=p.receipt",
        params![connection, delivery, part, attempt],
        |row| Ok((row.get::<_, String>(0)?, optional_decode::<Sender>(row, 1)?,
            decode::<DeliveryPart>(row, 2)?, decode::<EffectReceipt>(row, 3)?, decode::<ProvisionScope>(row, 4)?)),
    ).optional()?;
    let Some((route, sender, part, receipt, scope)) = evidence else {
        return Ok(None);
    };
    if route != scope.parent
        || part.room.thread.is_some()
        || !matches!(part.effect.operation, EffectOperation::Provision { .. })
        || receipt.kind != ReceiptKind::Destination
        || !valid_id(&receipt.id)
    {
        return Ok(None);
    }
    let Some(parent) = read_route(db, &scope.parent)? else {
        return Ok(None);
    };
    let binding = routes::conversation(db, &parent)?;
    if parent.room != part.room
        || !valid_routes(connection, &parent, &binding)
        || parent.revision != scope.parent_revision
        || binding.id != scope.binding
        || binding.revision != scope.binding_revision
    {
        return Ok(None);
    }
    let room = Room {
        thread: Some(receipt.id),
        ..parent.room.clone()
    };
    Ok(Some(ProvisionedDestination {
        scope,
        parent,
        binding,
        room,
        sender,
        effect: part.effect,
    }))
}

impl Store {
    pub(super) fn provisioned_destination(
        &self,
        connection: &str,
        delivery: &str,
        attempt: (u32, u32),
    ) -> Result<Option<ProvisionedDestination>> {
        self.read(|db| provisioned_destination(db, connection, delivery, attempt))
    }

    pub(super) fn bind_provisioned_destination(
        &self,
        delivery: &str,
        attempt: (u32, u32),
        authorized: &ProvisionedDestination,
        destination: &Route,
        generation: &str,
        policy: &AccessPolicy,
    ) -> Result<()> {
        self.write(|tx| {
            let connection = &destination.connection;
            ensure!(
                provisioned_destination(tx, connection, delivery, attempt)?.as_ref()
                    == Some(authorized),
                "Provisioned destination scope changed"
            );
            let parent = &authorized.parent;
            ensure!(
                authorized.sender.is_some() || routes::allows_notification(tx, &parent.id, now())?,
                "The parent destination write permission expired"
            );
            for (room, required) in [
                (&parent.room, true),
                (&authorized.binding.room, false),
                (&destination.room, false),
            ] {
                let required_protection = required
                    .then_some(authorized.effect.protection.as_deref())
                    .flatten();
                ensure!(
                    eligible_consent(
                        tx,
                        connection,
                        room,
                        required && authorized.effect.consent_required
                    )? && protection_matches(
                        tx,
                        connection,
                        generation,
                        room,
                        [
                            policy.delivery_policy(room).protection.as_deref(),
                            required_protection
                        ]
                    )?,
                    "Provisioned destination delivery policy changed"
                );
            }
            ensure!(
                read_route(tx, &destination.id)?
                    .is_none_or(|route| !route.blocked && route.revision == 0),
                "Provisioned destination has changed"
            );
            routes::insert_route(tx, destination)?;
            routes::share_conversation(tx, destination, &authorized.binding)
        })
    }
}
