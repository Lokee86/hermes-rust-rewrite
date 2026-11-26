use super::{
    Disposition, GatewayError, Handoff, HandoffItem, Ingress, IntakeReceipt,
    runtime::{blocking, db, now},
    store,
};
use crate::lock;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

const MAX_UNACKNOWLEDGED: usize = 128;
const MAX_UNACKNOWLEDGED_BYTES: usize = 16 * 1024 * 1024;

/// Verified metadata about one transport delivery. Its token stays outside the
/// durable event and is replaced independently on redelivery.
pub struct AcknowledgementLease {
    pub token: String,
    pub expires_ms: i64,
    pub payload_bytes: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum AcknowledgementOperation {
    Acknowledge,
    Extend,
}

pub struct LeasedAcknowledgement {
    ingress: Ingress,
    reservation: Reservation,
    token: String,
    pub receipt: IntakeReceipt,
}

impl Ingress {
    /// Reserve a bounded transport delivery and commit its input or refusal.
    /// A failure leaves the provider item available for redelivery.
    pub async fn stage_leased_input(
        &self,
        item: HandoffItem,
        lease: AcknowledgementLease,
    ) -> Result<LeasedAcknowledgement, GatewayError> {
        if !item.valid()
            || lease.token.is_empty()
            || lease.token.len() > 64 * 1024
            || lease.expires_ms <= now()
            || lease.payload_bytes > MAX_UNACKNOWLEDGED_BYTES
        {
            return Err(GatewayError::Invalid);
        }
        let ingress = self.clone();
        let reservation = self.checked(true, |_| {
            let book = self.acknowledgements();
            let id = crate::new_id();
            let mut active = lock(&book);
            if active.generation != self.generation {
                active.generation.clone_from(&self.generation);
                active.entries.clear();
            }
            active.entries.retain(|_, entry| entry.expires > now());
            let mut count = 0;
            let mut bytes = 0;
            for (event, entry) in &active.entries {
                if event != &item.event.id {
                    count += 1;
                    bytes += entry.bytes;
                }
            }
            if count >= MAX_UNACKNOWLEDGED || bytes + lease.payload_bytes > MAX_UNACKNOWLEDGED_BYTES
            {
                return Err(GatewayError::TemporarilyUnavailable);
            }
            active.entries.insert(
                item.event.id.clone(),
                Outstanding {
                    id: id.clone(),
                    expires: lease.expires_ms,
                    bytes: lease.payload_bytes,
                },
            );
            drop(active);
            Ok(Reservation {
                book,
                event: item.event.id.clone(),
                id,
            })
        })?;
        let evidence = db(store::fingerprint(&item.event))?;
        let id = db(store::key(
            "leased-intake",
            &(&item.event.id, evidence, &item.preparation),
        ))?;
        let receipt = self
            .stage_handoff(Handoff {
                id,
                items: vec![item],
                advance: None,
            })
            .await?
            .receipts
            .into_iter()
            .next()
            .ok_or(GatewayError::TemporarilyUnavailable)?;
        Ok(LeasedAcknowledgement {
            ingress,
            reservation,
            token: lease.token,
            receipt,
        })
    }
}

impl LeasedAcknowledgement {
    /// Check immediately before using the token for the corresponding provider
    /// operation. A superseded, expired, or disconnected lease is unusable.
    pub async fn token_for(
        &self,
        operation: AcknowledgementOperation,
    ) -> Result<Option<&str>, GatewayError> {
        let scope = self.ingress.clone();
        let event = self.reservation.event.clone();
        let id = self.reservation.id.clone();
        let conflict = self.receipt.disposition == Disposition::Conflict;
        let usable = blocking(move || {
            scope.checked(true, |state| {
                let book = scope.acknowledgements();
                let active = lock(&book);
                if !active
                    .entries
                    .get(&event)
                    .is_some_and(|entry| entry.id == id && entry.expires > now())
                {
                    return Ok(false);
                }
                let receipt = db(scope
                    .store
                    .read(|tx| store::intake_receipt(tx, &scope.connection, &event)))?;
                let Some(receipt) = receipt else {
                    return Ok(false);
                };
                let pending = receipt.disposition == Disposition::Deferred && !conflict;
                match operation {
                    AcknowledgementOperation::Acknowledge => Ok(!pending),
                    AcknowledgementOperation::Extend => {
                        Ok(pending && scope.can_prepare(state, &event)?)
                    }
                }
            })
        })
        .await?;
        Ok(usable.then_some(self.token.as_str()))
    }

    /// Record the provider's successful extension for this same token.
    pub async fn record_extension(&self, expires_ms: i64) -> Result<bool, GatewayError> {
        if self.receipt.disposition != Disposition::Deferred {
            return Ok(false);
        }
        let scope = self.ingress.clone();
        let event = self.reservation.event.clone();
        let id = self.reservation.id.clone();
        blocking(move || {
            scope.checked(true, |state| {
                let book = scope.acknowledgements();
                let mut active = lock(&book);
                let Some(entry) = active.entries.get_mut(&event) else {
                    return Ok(false);
                };
                if entry.id != id
                    || entry.expires <= now()
                    || expires_ms <= entry.expires
                    || !scope.can_prepare(state, &event)?
                {
                    return Ok(false);
                }
                entry.expires = expires_ms;
                Ok(true)
            })
        })
        .await
    }
}

#[derive(Default)]
pub(super) struct Acknowledgements {
    generation: String,
    entries: BTreeMap<String, Outstanding>,
}

struct Outstanding {
    id: String,
    expires: i64,
    bytes: usize,
}

struct Reservation {
    book: Arc<Mutex<Acknowledgements>>,
    event: String,
    id: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut active = lock(&self.book);
        if active
            .entries
            .get(&self.event)
            .is_some_and(|entry| entry.id == self.id)
        {
            active.entries.remove(&self.event);
        }
    }
}
