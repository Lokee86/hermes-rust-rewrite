use super::*;
use crate::messaging::{DeliveryPolicy, VerifiedInstallation};

type Render = dyn Fn(&DeliveryIntent) -> Vec<DeliveryPart> + Send + Sync;

#[derive(Clone)]
struct EffectFixture {
    wire: Fixture,
    render: Arc<Render>,
}

fn stock(
    driver: &Driver,
    policy: AccessPolicy,
    capabilities: EffectCapabilities,
    render: impl Fn(&DeliveryIntent) -> Vec<DeliveryPart> + Send + Sync + 'static,
) -> AdapterConfig {
    let mut wire = driver.fixture.clone();
    wire.capabilities.effects = capabilities;
    let mut config = driver.config(policy);
    config.factory = Arc::new(EffectFixture {
        wire,
        render: Arc::new(render),
    });
    config
}

impl AdapterFactory for EffectFixture {
    fn descriptor(&self) -> Descriptor {
        self.wire.descriptor()
    }

    fn connect(
        &self,
        _: CancellationToken,
    ) -> AdapterFuture<'_, Result<VerifiedAdapter, ConnectionFailure>> {
        Box::pin(async {
            Ok(VerifiedAdapter {
                account: self.wire.account.clone(),
                adapter: Arc::new(self.clone()),
            })
        })
    }
}

impl Adapter for EffectFixture {
    fn verified_installation(&self) -> Option<VerifiedInstallation> {
        Some(VerifiedInstallation {
            installation: "workspace".into(),
            tenant: None,
            application: None,
            receiver: None,
            origin: Some("https://fixture.example".into()),
            device: Some("device".into()),
            local_client: None,
        })
    }

    fn listen(
        &self,
        ingress: Ingress,
        cancel: CancellationToken,
    ) -> AdapterFuture<'_, ConnectionFailure> {
        self.wire.listen(ingress, cancel)
    }

    fn prepare(&self, intent: &DeliveryIntent) -> Result<Vec<DeliveryPart>, PreparationFailure> {
        Ok((self.render)(intent))
    }

    fn send(
        &self,
        attempt: SendAttempt,
        ingress: Ingress,
        cancel: CancellationToken,
    ) -> AdapterFuture<'_, DeliveryResult> {
        self.wire.send(attempt, ingress, cancel)
    }
}

fn part(intent: &DeliveryIntent, effect: Effect) -> DeliveryPart {
    DeliveryPart {
        room: intent.room.clone(),
        body: intent.body.clone(),
        effect,
        edit_target: None,
    }
}

fn typed(kind: ReceiptKind, id: &str) -> DeliveryResult {
    DeliveryResult::EffectConfirmed(EffectReceipt {
        kind,
        id: id.into(),
    })
}

fn now() -> i64 {
    crate::messaging::runtime::now()
}

async fn phase(gateway: &RunningGateway, id: &str, ordinal: u32, part: &str) -> Value {
    status(gateway, |value| {
        delivery(value, id, ordinal).is_some_and(|row| row["part_state"] == part)
    })
    .await
}

fn receipt_for(
    event: &str,
    room: &Room,
    identity: EffectIdentity,
    user: Option<&str>,
    provider: &str,
    state: ProviderDeliveryState,
) -> EffectObservation {
    EffectObservation {
        event: event.into(),
        room: room.clone(),
        identity,
        recipient: user.map(str::to_owned),
        observed_at_ms: now(),
        state,
        receipt: Some(provider.into()),
    }
}

fn group_plan(intent: &DeliveryIntent) -> Vec<DeliveryPart> {
    vec![part(
        intent,
        Effect {
            recipients: ["alice", "bob", "chris"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            ..Effect::default()
        },
    )]
}

#[tokio::test]
async fn group_partial_result_and_late_status_are_scoped_across_restart() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Push, "group-outbox", true);
    let config = || {
        stock(
            &driver,
            policy(),
            EffectCapabilities {
                recipient_subsets: true,
                ..EffectCapabilities::default()
            },
            group_plan,
        )
    };
    let gateway = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    driver
        .push(InboundEvent {
            room: Room {
                kind: RoomKind::Group,
                chat: "group".into(),
                thread: None,
            },
            ..dm("group-input", "ignored", "hello")
        })
        .await;
    let first = driver.sent().await;
    let room = first.attempt.payload.room.clone();
    let id = first.attempt.delivery_id.clone();
    assert_eq!(first.attempt.effect.recipients.len(), 3);
    let early = receipt_for(
        "early",
        &room,
        EffectIdentity::ProviderId("group-provider".into()),
        Some("alice"),
        "a",
        ProviderDeliveryState::Read,
    );
    assert_eq!(
        ingress.observe_delivery(early.clone()).await.unwrap(),
        ObservationResult::Pending
    );
    first.finish(DeliveryResult::Recipients(RecipientResults {
        receipt: Some("group-provider".into()),
        retry_after: Some(Duration::from_millis(1)),
        recipients: vec![
            RecipientReceipt {
                recipient: "alice".into(),
                outcome: RecipientOutcome::Accepted {
                    receipt: "a".into(),
                },
            },
            RecipientReceipt {
                recipient: "bob".into(),
                outcome: RecipientOutcome::Uncertain,
            },
            RecipientReceipt {
                recipient: "chris".into(),
                outcome: RecipientOutcome::NotAccepted,
            },
        ],
    }));
    let state = phase(&gateway, &id, 0, "uncertain").await;
    let row = delivery(&state, &id, 0).unwrap();
    assert_eq!(row["provider_status"][0]["state"], "read");
    assert_eq!(row["recipients"]["alice"]["accepted"]["receipt"], "a");
    gateway.shutdown().await.unwrap();

    let reopened = model.gateway(root.path(), vec![config()]).await;
    let reopened_ingress = driver.ready().await;
    assert_eq!(
        ingress.observe_delivery(early.clone()).await,
        Err(GatewayError::StaleConnection)
    );
    assert_eq!(
        reopened_ingress.observe_delivery(early).await.unwrap(),
        ObservationResult::Repeated
    );
    let wrong = receipt_for(
        "wrong",
        &Room {
            chat: "other".into(),
            ..room.clone()
        },
        EffectIdentity::ProviderId("group-provider".into()),
        Some("bob"),
        "b",
        ProviderDeliveryState::Delivered,
    );
    assert_eq!(
        reopened_ingress.observe_delivery(wrong).await.unwrap(),
        ObservationResult::Pending
    );
    assert!(
        timeout(Duration::from_millis(250), driver.sent.recv())
            .await
            .is_err()
    );
    let late = receipt_for(
        "late",
        &room,
        EffectIdentity::ProviderId("group-provider".into()),
        Some("bob"),
        "b",
        ProviderDeliveryState::Delivered,
    );
    assert_eq!(
        reopened_ingress.observe_delivery(late).await.unwrap(),
        ObservationResult::Applied
    );
    let second = driver.sent().await;
    assert_eq!(
        second.attempt.effect.recipients,
        ["chris".to_owned()].into_iter().collect()
    );
    assert!(!second.attempt.effect.retransmission);
    second.finish(DeliveryResult::Recipients(RecipientResults {
        receipt: Some("group-retry".into()),
        retry_after: None,
        recipients: vec![RecipientReceipt {
            recipient: "chris".into(),
            outcome: RecipientOutcome::Accepted {
                receipt: "c".into(),
            },
        }],
    }));
    phase(&reopened, &id, 0, "sent").await;
    let failure = receipt_for(
        "failed",
        &room,
        EffectIdentity::ProviderId("c".into()),
        Some("chris"),
        "c",
        ProviderDeliveryState::Failed,
    );
    assert_eq!(
        reopened_ingress.observe_delivery(failure).await.unwrap(),
        ObservationResult::Applied
    );
    let value = reopened.messaging_status().await.unwrap();
    let row = delivery(&value, &id, 0).unwrap();
    assert_eq!(row["state"], "sent");
    assert!(
        row["provider_status"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["recipient"] == "chris" && item["state"] == "failed")
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_consent_revocation_survives_restart_and_old_opt_in() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Poll, "consent", true);
    let config = || {
        stock(&driver, policy(), EffectCapabilities::default(), |intent| {
            [0, 1]
                .into_iter()
                .map(|_| {
                    part(
                        intent,
                        Effect {
                            consent_required: true,
                            ..Effect::default()
                        },
                    )
                })
                .collect()
        })
    };
    let gateway = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    let event = dm("consent-input", "consent-chat", "hello");
    let room = event.room.clone();
    let before = ConsentUpdate {
        event: "opt-in".into(),
        room: room.clone(),
        observed_at_ms: now(),
        eligible: true,
    };
    assert!(
        ingress
            .update_delivery_consent(before.clone())
            .await
            .unwrap()
    );
    driver.push(event).await;
    let first = driver.sent().await;
    let id = first.attempt.delivery_id.clone();
    assert!(
        ingress
            .update_delivery_consent(ConsentUpdate {
                event: "opt-out".into(),
                room,
                observed_at_ms: now(),
                eligible: false,
            })
            .await
            .unwrap()
    );
    first.finish(confirmed("first"));
    phase(&gateway, &id, 1, "blocked").await;
    gateway.shutdown().await.unwrap();

    let reopened = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    let repeated = ConsentUpdate {
        observed_at_ms: now() + 1,
        ..before
    };
    assert!(!ingress.update_delivery_consent(repeated).await.unwrap());
    let state = phase(&reopened, &id, 1, "blocked").await;
    assert_eq!(
        delivery(&state, &id, 1).unwrap()["error"],
        "eligibility_consent"
    );
    assert!(
        timeout(Duration::from_millis(250), driver.sent.recv())
            .await
            .is_err()
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn last_reply_grant_slot_stays_bound_after_a_lost_response() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Push, "reply-grant", true);
    let config = || {
        stock(&driver, policy(), EffectCapabilities::default(), |intent| {
            ["first", "second"]
                .into_iter()
                .map(|key| {
                    part(
                        intent,
                        Effect {
                            operation_key: Some(key.into()),
                            reply_grant: Some("grant".into()),
                            ..Effect::default()
                        },
                    )
                })
                .collect()
        })
    };
    let gateway = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    let event = dm("grant-anchor", "grant-chat", "hello");
    let grant = ReplyGrant {
        id: "grant".into(),
        source_event: event.id.clone(),
        room: event.room.clone(),
        operation: ReplyOperation::Publish,
        observed_at_ms: now(),
        expires_at_ms: now() + 20_000,
        absolute_expires_at_ms: now() + 40_000,
        slots: 1,
    };
    assert!(!ingress.register_reply_grant(grant.clone()).await.unwrap());
    driver.push(event).await;
    assert!(ingress.register_reply_grant(grant.clone()).await.unwrap());
    let first = driver.sent().await;
    let id = first.attempt.delivery_id.clone();
    first.finish(DeliveryResult::Uncertain);
    phase(&gateway, &id, 0, "uncertain").await;
    gateway.shutdown().await.unwrap();

    let reopened = model.gateway(root.path(), vec![config()]).await;
    let fresh = driver.ready().await;
    let updated = ReplyGrant {
        observed_at_ms: now(),
        expires_at_ms: grant.expires_at_ms + 1,
        ..grant.clone()
    };
    assert!(!fresh.refresh_reply_grant(updated).await.unwrap());
    let observed = receipt_for(
        "first-remote",
        &grant.room,
        EffectIdentity::OperationKey("first".into()),
        None,
        "provider-effect",
        ProviderDeliveryState::Accepted,
    );
    assert_eq!(
        fresh.observe_delivery(observed).await.unwrap(),
        ObservationResult::Applied
    );
    let state = phase(&reopened, &id, 1, "blocked").await;
    assert_eq!(
        delivery(&state, &id, 1).unwrap()["error"],
        "eligibility_reply_slots_exhausted"
    );
    assert!(
        timeout(Duration::from_millis(250), driver.sent.recv())
            .await
            .is_err()
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn qualified_replay_keeps_attempt_and_effect_identity() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Push, "replay-effect", true);
    let replay_scope = ReplayScope {
        device: "device".into(),
        endpoint: "https://fixture.example".into(),
        domain: "known-continuity".into(),
    };
    let guarantee = ReplayGuarantee {
        key: "op-key".into(),
        scope: replay_scope.clone(),
        expires_at_ms: now() + 30_000,
    };
    let config = || {
        let guarantee = guarantee.clone();
        stock(
            &driver,
            policy(),
            EffectCapabilities {
                retransmission: true,
                ..EffectCapabilities::default()
            },
            move |intent| {
                vec![part(
                    intent,
                    Effect {
                        replay: Some(guarantee.clone()),
                        ..Effect::default()
                    },
                )]
            },
        )
    };
    let gateway = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    ingress
        .set_replay_scope(Some(replay_scope.clone()))
        .await
        .unwrap();
    driver
        .push(dm("replay-anchor", "replay-chat", "hello"))
        .await;
    let first = driver.sent().await;
    let original = first.attempt.clone();
    first.finish(DeliveryResult::Uncertain);
    phase(&gateway, &original.delivery_id, 0, "uncertain").await;
    gateway.shutdown().await.unwrap();

    let reopened = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    ingress
        .set_replay_scope(Some(ReplayScope {
            domain: "different-cache".into(),
            ..replay_scope.clone()
        }))
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(250), driver.sent.recv())
            .await
            .is_err()
    );
    ingress.set_replay_scope(Some(replay_scope)).await.unwrap();
    let retry = driver.sent().await;
    assert!(retry.attempt.effect.retransmission);
    assert_eq!(retry.attempt.delivery_id, original.delivery_id);
    assert_eq!(retry.attempt.attempt, original.attempt);
    assert_eq!(retry.attempt.payload, original.payload);
    retry.finish(confirmed("committed"));
    phase(&reopened, &original.delivery_id, 0, "sent").await;
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn write_only_completion_does_not_claim_remote_acceptance() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Push, "limited-link", true);
    let config = || {
        let mut owner = policy();
        owner.scope.direct.delivery = Some(DeliveryPolicy {
            submitted: true,
            ..DeliveryPolicy::default()
        });
        stock(
            &driver,
            owner,
            EffectCapabilities {
                submitted: true,
                ..EffectCapabilities::default()
            },
            |intent| vec![part(intent, Effect::default())],
        )
    };
    let gateway = model.gateway(root.path(), vec![config()]).await;
    driver.ready().await;
    driver.push(dm("write-only", "limited", "hello")).await;
    let first = driver.sent().await;
    let id = first.attempt.delivery_id.clone();
    first.finish(DeliveryResult::Submitted);
    let state = phase(&gateway, &id, 0, "submitted").await;
    assert_eq!(delivery(&state, &id, 0).unwrap()["state"], "submitted");
    assert_eq!(
        delivery(&state, &id, 0).unwrap()["provider_status"],
        json!([])
    );
    driver
        .push(dm("second-write", "limited", "send next"))
        .await;
    let second = driver.sent().await;
    let uncertain = second.attempt.delivery_id.clone();
    second.finish(DeliveryResult::Uncertain);
    phase(&gateway, &uncertain, 0, "uncertain").await;
    gateway.shutdown().await.unwrap();
    let after = model.gateway(root.path(), vec![config()]).await;
    driver.ready().await;
    phase(&after, &uncertain, 0, "uncertain").await;
    assert!(
        timeout(Duration::from_millis(250), driver.sent.recv())
            .await
            .is_err()
    );
    after.shutdown().await.unwrap();
}

fn staged_plan(intent: &DeliveryIntent) -> Vec<DeliveryPart> {
    let DeliveryBody::Reply(message) = &intent.body else {
        return vec![part(intent, Effect::default())];
    };
    let Some(artifact) = message.attachments.first() else {
        return vec![part(intent, Effect::default())];
    };
    let operations = [
        EffectOperation::Provision {
            resource: ProvisionResource::Thread,
        },
        EffectOperation::Upload {
            artifact: artifact.clone(),
            step: UploadStep::Begin,
        },
        EffectOperation::Upload {
            artifact: artifact.clone(),
            step: UploadStep::Chunk { begin: 1, index: 0 },
        },
        EffectOperation::Upload {
            artifact: artifact.clone(),
            step: UploadStep::Finish {
                begin: 1,
                chunks: vec![2],
            },
        },
        EffectOperation::Publish {
            using: vec![
                EffectDependency {
                    part: 0,
                    kind: ReceiptKind::Destination,
                },
                EffectDependency {
                    part: 3,
                    kind: ReceiptKind::File,
                },
            ],
        },
    ];
    operations
        .into_iter()
        .map(|operation| {
            part(
                intent,
                Effect {
                    operation,
                    ..Effect::default()
                },
            )
        })
        .collect()
}

#[tokio::test]
async fn resource_receipts_resume_upload_and_publish_in_order() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Push, "segmented-upload", true);
    let config = || {
        stock(
            &driver,
            policy(),
            EffectCapabilities {
                prepare_resources: true,
                ..EffectCapabilities::default()
            },
            staged_plan,
        )
    };
    let gateway = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    let route_id = driver
        .push(dm("staged-route", "upload-chat", "hello"))
        .await
        .route
        .unwrap();
    let warmup = driver.sent().await;
    warmup.finish(confirmed("warmup"));
    let route = ingress.store.route(&route_id).unwrap().unwrap();
    let file = ingress
        .store
        .put_attachment(&ingress.connection, "evidence.txt".into(), b"attachment")
        .unwrap();
    let id = "segmented-test";
    ingress
        .store
        .enqueue(
            &route,
            &DeliveryIntent {
                id: id.into(),
                room: route.room.clone(),
                reply_anchor: None,
                edit_target: None,
                body: DeliveryBody::Reply(Message {
                    text: "ship this".into(),
                    attachments: vec![file],
                }),
            },
        )
        .unwrap();
    let reserve = driver.sent().await;
    reserve.finish(typed(ReceiptKind::Destination, "provisioned"));
    let begin = driver.sent().await;
    begin.finish(typed(ReceiptKind::UploadSession, "upload-session"));
    let chunk = driver.sent().await;
    assert_eq!(chunk.attempt.effect.dependencies[0].1.id, "upload-session");
    chunk.finish(DeliveryResult::Uncertain);
    phase(&gateway, id, 2, "uncertain").await;
    gateway.shutdown().await.unwrap();

    let reopened = model.gateway(root.path(), vec![config()]).await;
    let ingress = driver.ready().await;
    let old = ingress
        .uncertain_deliveries()
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.delivery_id == id)
        .unwrap();
    assert!(
        ingress
            .reconcile_delivery(old, typed(ReceiptKind::UploadChunk, "chunk-confirmed"))
            .await
            .unwrap()
    );
    let finish = driver.sent().await;
    assert_eq!(
        finish
            .attempt
            .effect
            .dependencies
            .iter()
            .map(|effect| effect.1.id.as_str())
            .collect::<Vec<_>>(),
        ["upload-session", "chunk-confirmed"]
    );
    finish.finish(typed(ReceiptKind::File, "provider-file"));
    let publish = driver.sent().await;
    assert_eq!(
        publish
            .attempt
            .effect
            .dependencies
            .iter()
            .map(|effect| effect.1.id.as_str())
            .collect::<Vec<_>>(),
        ["provisioned", "provider-file"]
    );
    publish.finish(confirmed("submitted-message"));
    phase(&reopened, id, 4, "sent").await;
    reopened.shutdown().await.unwrap();
}
