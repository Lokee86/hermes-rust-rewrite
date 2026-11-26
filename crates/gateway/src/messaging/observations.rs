use super::{
    ActorKind, GatewayError, InboundBody, InboundEvent, Ingress, IntakeReceipt, Message, Room,
    Sender,
    runtime::{blocking, db},
    store::{self, Candidate},
};

const OBSERVATION: &str = "connection-observation";

pub(super) fn is_observation(id: &str) -> bool {
    id.starts_with("connection-observation-")
}

/// A message observed on the leased transport when it supplies no durable
/// provider message ID. The adapter assigns a bounded per-connection ordinal.
pub struct ConnectionObservation {
    pub ordinal: u64,
    pub room: Room,
    pub principal: Sender,
    pub mentioned: bool,
    pub message: Message,
}

impl Ingress {
    pub async fn submit_observation(
        &self,
        observation: ConnectionObservation,
    ) -> Result<IntakeReceipt, GatewayError> {
        let scope = self.clone();
        let result = blocking(move || {
            scope.checked(true, |state| {
                if !scope
                    .authorized_identity(&state.policy, &observation.room, &observation.principal)?
                    .is_some_and(|(_, kind)| kind == ActorKind::Connection)
                {
                    return Err(GatewayError::Forbidden);
                }
                let mut event = InboundEvent {
                    id: db(store::key(
                        OBSERVATION,
                        &(&scope.generation, observation.ordinal),
                    ))?,
                    room: observation.room,
                    sender: observation.principal,
                    reply_anchor: None,
                    mentioned: observation.mentioned,
                    body: InboundBody::Message(observation.message),
                };
                let decision = scope.route_event(&state.policy, &event)?;
                scope.normalize_event(&mut event)?;
                let result = db(scope.store.intake(
                    &scope.connection,
                    state.revision,
                    vec![Candidate { event, decision }],
                    None,
                ))?
                .ok_or(GatewayError::CheckpointChanged)?;
                result
                    .receipts
                    .into_iter()
                    .next()
                    .ok_or(GatewayError::TemporarilyUnavailable)
            })
        })
        .await?;
        self.notify();
        Ok(result)
    }
}
