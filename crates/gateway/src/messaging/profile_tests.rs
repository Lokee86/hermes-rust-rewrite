use super::*;
use crate::{Gateway, GatewayConfig, RunningGateway};
use std::{path::Path, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Fixture {
    name: String,
    proof: VerifiedInstallation,
    account: String,
    credential_required: bool,
    ready: async_channel::Sender<Ingress>,
}

impl AdapterFactory for Fixture {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            platform: "proof-fixture".into(),
            version: CONTRACT_VERSION,
            capabilities: Capabilities::TEXT,
        }
    }

    fn connect(
        &self,
        _: CancellationToken,
    ) -> AdapterFuture<'_, Result<VerifiedAdapter, ConnectionFailure>> {
        Box::pin(async {
            if self.credential_required {
                Err(ConnectionFailure::Credentials)
            } else {
                Ok(VerifiedAdapter {
                    account: self.account.clone(),
                    adapter: Arc::new(self.clone()),
                })
            }
        })
    }

    fn connect_enrolled(
        &self,
        credential: EnrollmentCredential,
        _: CancellationToken,
    ) -> AdapterFuture<'_, Result<VerifiedAdapter, ConnectionFailure>> {
        Box::pin(async move {
            let account = match credential.secret() {
                "owner-secret" | "renewed-secret" => self.account.clone(),
                "other-secret" => "different-account".into(),
                _ => return Err(ConnectionFailure::Credentials),
            };
            Ok(VerifiedAdapter {
                account,
                adapter: Arc::new(self.clone()),
            })
        })
    }
}

impl Adapter for Fixture {
    fn verified_installation(&self) -> Option<VerifiedInstallation> {
        Some(self.proof.clone())
    }

    fn listen(
        &self,
        ingress: Ingress,
        cancel: CancellationToken,
    ) -> AdapterFuture<'_, ConnectionFailure> {
        Box::pin(async move {
            if self.ready.send(ingress).await.is_err() {
                return ConnectionFailure::Stopped;
            }
            cancel.cancelled().await;
            ConnectionFailure::Stopped
        })
    }

    fn prepare(&self, _: &DeliveryIntent) -> Result<Vec<DeliveryPart>, PreparationFailure> {
        Err(PreparationFailure::Unsupported)
    }

    fn send(
        &self,
        _: SendAttempt,
        _: Ingress,
        _: CancellationToken,
    ) -> AdapterFuture<'_, DeliveryResult> {
        Box::pin(std::future::ready(DeliveryResult::Rejected))
    }
}

fn fixture(name: &str, tenant: &str) -> (Fixture, async_channel::Receiver<Ingress>) {
    let (ready, receive) = async_channel::unbounded();
    (
        Fixture {
            name: name.into(),
            account: "provider-account".into(),
            credential_required: false,
            proof: VerifiedInstallation {
                application: Some(name.into()),
                tenant: Some(tenant.into()),
                installation: "provider-installation".into(),
                receiver: Some("public-receiver".into()),
                origin: Some("https://provider.example".into()),
                device: Some("device-one".into()),
                local_client: None,
            },
            ready,
        },
        receive,
    )
}

async fn open(root: &Path, factories: &[Fixture], policy: &AccessPolicy) -> RunningGateway {
    let mut config = GatewayConfig::new(
        root,
        hermes_engine::Config {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "fixture".into(),
            api_key: String::new(),
        },
    );
    config.adapters = factories
        .iter()
        .map(|factory| AdapterConfig {
            name: factory.name.clone(),
            factory: Arc::new(factory.clone()),
            policy: policy.clone(),
        })
        .collect();
    Gateway::start(config).await.unwrap()
}

async fn ready(receiver: &async_channel::Receiver<Ingress>) -> Ingress {
    timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

fn principal(id: &str) -> Sender {
    Sender {
        id: id.into(),
        roles: Default::default(),
    }
}

fn room(chat: &str) -> Room {
    Room {
        kind: RoomKind::Direct,
        chat: chat.into(),
        thread: None,
    }
}

fn message(id: &str, sender: Sender, room: Room) -> InboundEvent {
    InboundEvent {
        id: id.into(),
        room,
        sender,
        reply_anchor: None,
        mentioned: true,
        body: InboundBody::Message(Message::text("fixture")),
    }
}

fn person(actor: &str, room: Room) -> ActorEvidence {
    ActorEvidence {
        actor: actor.into(),
        kind: ActorKind::Person,
        revision: 1,
        expires_at: super::profile::time_ms() + 60_000,
        aliases: Vec::new(),
        memberships: vec![MemberEvidence {
            room,
            roles: Default::default(),
        }],
    }
}

fn owner_policy() -> AccessPolicy {
    let mut result = AccessPolicy {
        users: ["canonical".into()].into(),
        ..Default::default()
    };
    result.scope.require_actor_evidence = true;
    result
}

#[tokio::test]
async fn owner_enrollment_expires_rotates_and_survives_restart() {
    let temp = TempDir::new().unwrap();
    let (mut factory, queue) = fixture("enrolled", "tenant");
    factory.credential_required = true;
    let mut policy = owner_policy();
    let gateway = open(temp.path(), &[factory.clone()], &policy).await;
    let expired = gateway
        .begin_adapter_enrollment(&factory.name, Duration::from_millis(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let secret = EnrollmentCredential::new("owner-secret".into()).unwrap();
    assert!(
        gateway
            .complete_adapter_enrollment(&factory.name, &expired.id, secret.clone())
            .await
            .is_err()
    );

    let lease = gateway
        .begin_adapter_enrollment(&factory.name, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(
        gateway
            .complete_adapter_enrollment(
                &factory.name,
                &lease.id,
                EnrollmentCredential::new("wrong".into()).unwrap(),
            )
            .await
            .is_err()
    );
    gateway
        .complete_adapter_enrollment(&factory.name, &lease.id, secret.clone())
        .await
        .unwrap();
    let first = ready(&queue).await;
    let before = gateway.adapter_settings().await.unwrap().remove(0);
    assert!(before.credential_ref.is_some());
    assert!(
        !serde_json::to_string(&before)
            .unwrap()
            .contains(secret.secret())
    );

    let renewed = gateway
        .begin_adapter_enrollment(&factory.name, Duration::from_secs(30))
        .await
        .unwrap();
    gateway
        .complete_adapter_enrollment(
            &factory.name,
            &renewed.id,
            EnrollmentCredential::new("renewed-secret".into()).unwrap(),
        )
        .await
        .unwrap();
    let second = ready(&queue).await;
    assert_eq!(first.connection, second.connection);
    assert_ne!(
        gateway.adapter_settings().await.unwrap()[0].credential_ref,
        before.credential_ref
    );
    assert_eq!(
        first.checkpoint("poll".into()).await.unwrap_err(),
        GatewayError::StaleConnection
    );
    assert!(
        gateway
            .complete_adapter_enrollment(&factory.name, &lease.id, secret)
            .await
            .is_err()
    );

    policy.scope.denied_users.insert("canonical".into());
    gateway
        .set_adapter_policy(&factory.name, policy.clone())
        .await
        .unwrap();
    gateway.enable_adapter(&factory.name, false).await.unwrap();
    gateway.shutdown().await.unwrap();
    let gateway = open(temp.path(), &[factory.clone()], &owner_policy()).await;
    let saved = gateway.adapter_settings().await.unwrap().remove(0);
    assert!(!saved.enabled);
    assert_eq!(saved.policy, policy);
    assert!(
        timeout(Duration::from_millis(50), queue.recv())
            .await
            .is_err()
    );
    gateway.enable_adapter(&factory.name, true).await.unwrap();
    let resumed = ready(&queue).await;
    assert_eq!(resumed.connection, first.connection);

    let replacement = gateway
        .begin_adapter_enrollment(&factory.name, Duration::from_secs(30))
        .await
        .unwrap();
    gateway
        .complete_adapter_enrollment(
            &factory.name,
            &replacement.id,
            EnrollmentCredential::new("other-secret".into()).unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(ready(&queue).await.connection, first.connection);
    assert_eq!(
        resumed.checkpoint("poll".into()).await.unwrap_err(),
        GatewayError::StaleConnection
    );
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn receiver_routes_exactly_one_verified_installation_and_fences_old_routes() {
    let temp = TempDir::new().unwrap();
    let (a, qa) = fixture("app-a", "tenant-a");
    let (b, qb) = fixture("app-b", "tenant-a");
    let (c, qc) = fixture("app-c", "tenant-b");
    let gateway = open(
        temp.path(),
        &[a.clone(), b.clone(), c.clone()],
        &owner_policy(),
    )
    .await;
    let first = ready(&qa).await;
    let second = ready(&qb).await;
    let third = ready(&qc).await;
    let names = [a.name.clone(), b.name.clone(), c.name.clone()];
    let receiver = gateway
        .bind_adapter_receiver("public-receiver", &names)
        .await
        .unwrap();
    let ids = [
        InstallationKey::from(&a.proof),
        InstallationKey::from(&b.proof),
        InstallationKey::from(&c.proof),
    ];
    assert_eq!(
        receiver
            .route_verified(&[ids[0].clone()])
            .unwrap()
            .connection,
        first.connection
    );
    assert_eq!(
        receiver
            .route_verified(&[ids[1].clone()])
            .unwrap()
            .connection,
        second.connection
    );
    assert_eq!(
        receiver
            .route_verified(&[ids[2].clone()])
            .unwrap()
            .connection,
        third.connection
    );
    assert!(matches!(
        receiver.route_verified(&ids[..2]),
        Err(GatewayError::Forbidden)
    ));
    assert!(
        gateway
            .bind_adapter_receiver("public-receiver", &names)
            .await
            .is_err()
    );
    assert!(
        gateway
            .bind_adapter_receiver("different-receiver", &names)
            .await
            .is_err()
    );
    gateway.enable_adapter(&a.name, false).await.unwrap();
    assert!(matches!(
        receiver.route_verified(&[ids[0].clone()]),
        Err(GatewayError::StaleConnection)
    ));
    assert!(receiver.route_verified(&[ids[1].clone()]).is_ok());
    drop(receiver);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn verified_aliases_route_together_and_membership_changes_fence_access() {
    let temp = TempDir::new().unwrap();
    let (factory, queue) = fixture("alias", "tenant");
    let mut policy = owner_policy();
    policy.operators.insert("canonical".into());
    let gateway = open(temp.path(), &[factory.clone()], &policy).await;
    let ingress = ready(&queue).await;
    let room = room("private-chat");
    let first = ActorAlias {
        namespace: "user".into(),
        id: "ID".into(),
    };
    let second = ActorAlias {
        namespace: "directory".into(),
        id: "ID".into(),
    };
    let one = principal(&first.sender_id().unwrap());
    let two = principal(&second.sender_id().unwrap());
    let mut actor = person("canonical", room.clone());
    actor.aliases = vec![first, second];
    ingress.record_actor(actor.clone()).await.unwrap();
    let a = ingress
        .submit(
            vec![message("provider-event", one.clone(), room.clone())],
            None,
        )
        .await
        .unwrap();
    let b = ingress
        .submit(
            vec![message("provider-event", two.clone(), room.clone())],
            None,
        )
        .await
        .unwrap();
    assert_eq!(a.receipts[0].disposition, Disposition::Accepted);
    assert_eq!(a.receipts[0].route, b.receipts[0].route);
    assert!(b.receipts[0].repeated);
    let other = ingress
        .submit(
            vec![message("other-event", one.clone(), room.clone())],
            None,
        )
        .await
        .unwrap();
    assert!(!other.receipts[0].repeated);

    let claimed = ActorEvidence {
        actor: "impostor".into(),
        ..actor.clone()
    };
    assert_eq!(
        ingress.record_actor(claimed).await.unwrap_err(),
        GatewayError::Forbidden
    );
    let outsider = ingress
        .submit(
            vec![message("unknown", principal("unknown"), room.clone())],
            None,
        )
        .await
        .unwrap();
    assert_eq!(outsider.receipts[0].disposition, Disposition::Forbidden);
    assert!(ingress.authorize_operator(&policy, &room, &one).unwrap());

    actor.revision += 1;
    actor.aliases.pop();
    ingress.record_actor(actor.clone()).await.unwrap();
    let retired = ingress
        .submit(vec![message("retired", two, room.clone())], None)
        .await
        .unwrap();
    assert_eq!(retired.receipts[0].disposition, Disposition::Forbidden);
    actor.revision += 1;
    actor.memberships.clear();
    ingress.record_actor(actor).await.unwrap();
    assert!(!ingress.authorize_operator(&policy, &room, &one).unwrap());
    gateway.shutdown().await.unwrap();
    let gateway = open(temp.path(), &[factory], &policy).await;
    let ingress = ready(&queue).await;
    let revoked = ingress
        .submit(vec![message("after-reopen", one, room)], None)
        .await
        .unwrap();
    assert_eq!(revoked.receipts[0].disposition, Disposition::Forbidden);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn scoped_pairing_and_shared_principals_do_not_grant_control() {
    let temp = TempDir::new().unwrap();
    let (factory, queue) = fixture("pairing", "tenant");
    let mut policy = owner_policy();
    policy.scope.pair_direct = true;
    policy.users.insert("topic".into());
    policy.operators.insert("topic".into());
    let gateway = open(temp.path(), &[factory.clone()], &policy).await;
    let ingress = ready(&queue).await;
    let private = room("private");
    let topic = room("topic");
    ingress
        .record_actor(person("asker", private.clone()))
        .await
        .unwrap();
    ingress
        .record_actor(ActorEvidence {
            kind: ActorKind::Shared,
            ..person("topic", topic.clone())
        })
        .await
        .unwrap();
    let challenge = ingress
        .begin_pairing(private.clone(), principal("asker"))
        .await
        .unwrap();
    assert!(
        gateway
            .shared
            .messaging
            .pairing(&factory.name, &challenge.code, None)
            .await
            .unwrap()
    );
    assert!(
        !gateway
            .shared
            .messaging
            .pairing(&factory.name, &challenge.code, None)
            .await
            .unwrap()
    );
    let paired = ingress
        .submit(
            vec![message("paired", principal("asker"), private.clone())],
            None,
        )
        .await
        .unwrap();
    assert_eq!(paired.receipts[0].disposition, Disposition::Accepted);
    assert!(
        gateway
            .shared
            .messaging
            .pairing(&factory.name, "asker", Some(&private.chat))
            .await
            .unwrap()
    );
    assert!(
        !ingress
            .authorize_sender(&policy, &private, &principal("asker"))
            .unwrap()
    );

    let mut input = message("forged-control", principal("topic"), topic);
    input.body = InboundBody::Control(Control::Interrupt {
        execution_generation: 1,
    });
    let denied = ingress.submit(vec![input], None).await.unwrap();
    assert_eq!(denied.receipts[0].disposition, Disposition::Forbidden);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn delegated_authority_pins_actor_resource_origin_and_device_across_restart() {
    let temp = TempDir::new().unwrap();
    let (mut factory, queue) = fixture("transport", "tenant");
    let policy = owner_policy();
    let gateway = open(temp.path(), &[factory.clone()], &policy).await;
    let ingress = ready(&queue).await;
    let room = room("room");
    ingress
        .record_actor(person("canonical", room.clone()))
        .await
        .unwrap();
    let mut address = TransportAddress {
        reference: "grant".into(),
        endpoint: "https://provider.example/files/one".into(),
        resource: "file:1".into(),
        operation: "upload".into(),
        actor: Some("canonical".into()),
        room: Some(room),
        expires_at: super::profile::time_ms() + 60_000,
        revision: 1,
    };
    let mut use_ = TransportUse {
        reference: address.reference.clone(),
        endpoint: address.endpoint.clone(),
        resource: address.resource.clone(),
        operation: address.operation.clone(),
        actor: address.actor.clone(),
        room: address.room.clone(),
    };
    let secret = EnrollmentCredential::new("actor-credential".into()).unwrap();
    ingress
        .record_transport_address(address.clone(), Some(secret.clone()))
        .await
        .unwrap();
    assert_eq!(
        ingress
            .transport_authority(&use_)
            .await
            .unwrap()
            .credential(),
        Some("actor-credential")
    );
    let redirect = TransportUse {
        endpoint: "https://foreign.example/files/one".into(),
        ..use_.clone()
    };
    assert!(matches!(
        ingress.transport_authority(&redirect).await,
        Err(GatewayError::Forbidden)
    ));
    use_.actor = Some("other".into());
    assert!(matches!(
        ingress.transport_authority(&use_).await,
        Err(GatewayError::Forbidden)
    ));
    use_.actor = address.actor.clone();
    gateway.shutdown().await.unwrap();

    let gateway = open(temp.path(), &[factory.clone()], &policy).await;
    let ingress = ready(&queue).await;
    assert_eq!(
        ingress
            .transport_authority(&use_)
            .await
            .unwrap()
            .credential(),
        Some("actor-credential")
    );
    gateway.shutdown().await.unwrap();

    factory.proof.device = Some("device-two".into());
    let gateway = open(temp.path(), &[factory], &policy).await;
    let ingress = ready(&queue).await;
    assert!(matches!(
        ingress.transport_authority(&use_).await,
        Err(GatewayError::Forbidden)
    ));
    address.revision += 1;
    ingress
        .record_transport_address(address.clone(), Some(secret))
        .await
        .unwrap();
    assert!(ingress.transport_authority(&use_).await.is_ok());
    assert!(
        ingress
            .revoke_transport_address("grant", address.revision + 1)
            .await
            .unwrap()
    );
    assert!(matches!(
        ingress.transport_authority(&use_).await,
        Err(GatewayError::Forbidden)
    ));
    assert_eq!(
        ingress
            .record_transport_address(address, None)
            .await
            .unwrap_err(),
        GatewayError::Forbidden
    );
    gateway.shutdown().await.unwrap();
}
