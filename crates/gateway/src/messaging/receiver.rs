use super::{
    Disposition, Handoff, HandoffItem, Ingress, InstallationKey, IntakeReceipt, SharedReceiver,
    runtime::{MAX_BATCH, blocking, db, valid_id},
    store::{self, Store},
    types::GatewayError,
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_receiver_envelopes (
    receiver TEXT NOT NULL, envelope TEXT NOT NULL,
    fingerprint TEXT NOT NULL, bindings TEXT NOT NULL,
    PRIMARY KEY(receiver, envelope)
);
";

/// The matches are installations authenticated by the receiver for this item.
pub struct ReceiverItem {
    pub matches: Vec<InstallationKey>,
    pub input: HandoffItem,
}

impl SharedReceiver {
    /// Returns a positive acknowledgement only after every item has a durable
    /// disposition. If processing stops partway through, replay the same
    /// envelope against a current receiver lease to finish the remaining items.
    pub async fn stage_envelope(
        &self,
        id: String,
        items: Vec<ReceiverItem>,
    ) -> Result<Vec<IntakeReceipt>, GatewayError> {
        if !valid_id(&id)
            || items.is_empty()
            || items.len() > MAX_BATCH
            || items
                .iter()
                .any(|item| !item.input.valid() || item.matches.len() > MAX_BATCH)
        {
            return Err(GatewayError::Invalid);
        }
        let Some(live) = self
            .routes
            .iter()
            .map(|(_, ingress)| ingress)
            .find(|ingress| ingress.checked(true, |_| Ok(())).is_ok())
        else {
            return Err(GatewayError::StaleConnection);
        };
        let routes = items
            .iter()
            .map(|item| self.match_installation(&item.matches))
            .collect::<Vec<_>>();
        let proposed = db(routes
            .iter()
            .map(|route| route.map(binding).transpose())
            .collect::<Result<Vec<_>>>())?;
        let store = live.store.clone();
        let receiver = self.identity.clone();
        let envelope = id.clone();
        let (items, bindings, repeated) = blocking(move || {
            let fingerprints = items
                .iter()
                .map(|item| {
                    let mut matches = item.matches.iter().collect::<Vec<_>>();
                    matches.sort_unstable();
                    Ok((
                        matches,
                        &item.input.event.id,
                        store::fingerprint(&item.input.event)?,
                        &item.input.preparation,
                    ))
                })
                .collect::<Result<Vec<_>>>();
            let digest = db(fingerprints.and_then(|items| store::key("receiver-items", &items)))?;
            let (bindings, repeated) =
                db(store.bind_envelope(&receiver, &envelope, &digest, &proposed))?
                    .ok_or(GatewayError::Conflict)?;
            Ok((items, bindings, repeated))
        })
        .await?;

        let mut receipts = Vec::with_capacity(items.len());
        for (index, ((item, route), expected)) in
            items.into_iter().zip(routes).zip(bindings).enumerate()
        {
            let Some(expected) = expected else {
                receipts.push(IntakeReceipt {
                    id: item.input.event.id,
                    disposition: Disposition::Forbidden,
                    route: None,
                    repeated,
                });
                continue;
            };
            let route = route.ok_or(GatewayError::StaleConnection)?;
            if db(binding(route))? != expected {
                return Err(GatewayError::StaleConnection);
            }
            let response = route
                .stage_handoff(Handoff {
                    id: db(store::key("receiver-item", &(&self.identity, &id, index)))?,
                    items: vec![item.input],
                    advance: None,
                })
                .await?;
            receipts.extend(response.receipts);
        }
        Ok(receipts)
    }
}

fn binding(ingress: &Ingress) -> Result<String> {
    store::key(
        "receiver-binding",
        &(&ingress.connection, ingress.installation()),
    )
}

impl Store {
    fn bind_envelope(
        &self,
        receiver: &str,
        envelope: &str,
        fingerprint: &str,
        proposed: &[Option<String>],
    ) -> Result<Option<(Vec<Option<String>>, bool)>> {
        self.write(|tx| {
            let existing = tx
                .query_row(
                    "SELECT fingerprint,bindings FROM messaging_receiver_envelopes
                 WHERE receiver=?1 AND envelope=?2",
                    params![receiver, envelope],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            store::decode::<Vec<Option<String>>>(row, 1)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((stored, bindings)) = existing {
                return Ok((stored == fingerprint && bindings.len() == proposed.len())
                    .then_some((bindings, true)));
            }
            tx.execute(
                "INSERT INTO messaging_receiver_envelopes(receiver,envelope,fingerprint,bindings)
                 VALUES (?1,?2,?3,?4)",
                params![
                    receiver,
                    envelope,
                    fingerprint,
                    serde_json::to_string(proposed)?
                ],
            )?;
            Ok(Some((proposed.to_vec(), false)))
        })
    }
}
