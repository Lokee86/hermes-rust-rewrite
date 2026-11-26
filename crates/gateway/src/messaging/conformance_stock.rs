use super::*;

fn expires() -> i64 {
    chrono::Utc::now().timestamp_millis() + 60_000
}

fn handoff(id: &str, items: Vec<InboundEvent>, advance: Option<Advance>) -> Handoff {
    Handoff {
        id: id.into(),
        items: items
            .into_iter()
            .map(|event| HandoffItem {
                event,
                preparation: None,
            })
            .collect(),
        advance,
    }
}

#[tokio::test]
async fn durable_preparation_orders_routes_and_reopens_before_hydration() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Poll, "durable-handoff", false);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    let mut batch = handoff(
        "batch",
        vec![dm("deferred", "one", "")],
        Some(advance(None, "next")),
    );
    batch.items[0].preparation = Some(Preparation {
        token: "source-transfer".into(),
        purpose: PreparationPurpose::ProtectedContent,
        expires_ms: expires(),
    });
    let staged = ingress.stage_handoff(batch.clone()).await.unwrap();
    assert_eq!(staged.checkpoint.as_deref(), Some("next"));
    assert_eq!(staged.receipts[0].disposition, Disposition::Deferred);
    driver.push(dm("later", "one", "later")).await;
    driver.push(dm("other", "two", "other")).await;
    assert_eq!(reply(&driver.sent().await.attempt), "answer other");
    assert!(driver.sent.try_recv().is_err());

    gateway.shutdown().await.unwrap();
    let resumed = Driver::new(Transport::Poll, "durable-handoff", false);
    let gateway = model
        .gateway(root.path(), vec![resumed.config(policy())])
        .await;
    let ingress = resumed.ready().await;
    let repeat = ingress.stage_handoff(batch.clone()).await.unwrap();
    assert!(repeat.receipts[0].repeated);
    assert_eq!(repeat.receipts[0].disposition, Disposition::Deferred);
    let mut conflict = batch;
    conflict.items[0].preparation.as_mut().unwrap().token = "different".into();
    assert_eq!(
        ingress.stage_handoff(conflict).await.unwrap_err(),
        GatewayError::Conflict
    );
    assert_eq!(
        ingress.pending_preparations().await.unwrap()[0].event.id,
        "deferred"
    );
    assert_eq!(
        ingress
            .complete_preparation("deferred".into(), "forged".into(), Message::text("invalid"))
            .await
            .unwrap_err(),
        GatewayError::Forbidden
    );
    let done = ingress
        .complete_preparation(
            "deferred".into(),
            "source-transfer".into(),
            Message::text("hydrated"),
        )
        .await
        .unwrap();
    assert_eq!(done.disposition, Disposition::Accepted);
    assert!(
        ingress
            .complete_preparation(
                "deferred".into(),
                "source-transfer".into(),
                Message::text("hydrated")
            )
            .await
            .unwrap()
            .repeated
    );
    assert_eq!(
        ingress
            .complete_preparation(
                "deferred".into(),
                "source-transfer".into(),
                Message::text("different")
            )
            .await
            .unwrap_err(),
        GatewayError::Conflict
    );
    assert_eq!(reply(&resumed.sent().await.attempt), "answer hydrated");
    assert_eq!(reply(&resumed.sent().await.attempt), "answer later");
    assert_eq!(model.requests().len(), 3);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn repair_merges_live_and_history_in_source_order_after_restart() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Poll, "repair", false);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    driver
        .frame(
            vec![dm("baseline", "room", "baseline")],
            Some(advance(None, "known")),
        )
        .await
        .unwrap();
    driver.sent().await;
    let older = dm("earlier", "room", "earlier");
    let newer = dm("newer", "room", "newer");
    let window = RepairWindow {
        id: "gap".into(),
        stream: "inbox".into(),
        room: older.room.clone(),
        last_committed: "known".into(),
        expires_ms: expires(),
        limit: 8,
    };
    ingress.begin_repair(window).await.unwrap();
    let first = RepairPage {
        id: "first".into(),
        previous: None,
        items: vec![RepairItem {
            source_order: 20,
            event: newer.clone(),
        }],
    };
    ingress
        .stage_repair_page("gap".into(), first.clone())
        .await
        .unwrap();
    assert_eq!(
        ingress.submit(vec![newer.clone()], None).await.unwrap_err(),
        GatewayError::CheckpointChanged
    );
    assert!(driver.sent.try_recv().is_err());
    gateway.shutdown().await.unwrap();
    let driver = Driver::new(Transport::Poll, "repair", false);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    assert_eq!(ingress.pending_repairs().await.unwrap()[0].staged, 1);
    assert_eq!(
        ingress
            .stage_repair_page("gap".into(), first)
            .await
            .unwrap()
            .staged,
        1
    );
    let second = RepairPage {
        id: "second".into(),
        previous: Some("first".into()),
        items: vec![
            RepairItem {
                source_order: 10,
                event: older,
            },
            RepairItem {
                source_order: 20,
                event: newer,
            },
        ],
    };
    assert_eq!(
        ingress
            .stage_repair_page("gap".into(), second)
            .await
            .unwrap()
            .staged,
        2
    );
    let recovered = ingress
        .finish_repair("gap".into(), Some(advance(Some("known"), "repaired")))
        .await
        .unwrap();
    assert!(!recovered.gap);
    assert_eq!(recovered.checkpoint.as_deref(), Some("repaired"));
    assert_eq!(reply(&driver.sent().await.attempt), "answer earlier");
    assert_eq!(reply(&driver.sent().await.attempt), "answer newer");
    assert_eq!(model.requests().len(), 3);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn shared_conversation_keeps_destinations_and_command_outcomes_scoped() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Push, "shared-destinations", false);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    let first = dm("first", "one", "first");
    let second = dm("second", "two", "second");
    let anchor = ingress
        .verify_destination(first.room.clone(), expires())
        .await
        .unwrap();
    let member = ingress
        .verify_destination(second.room.clone(), expires())
        .await
        .unwrap();
    gateway
        .bind_messaging_conversation(&member, &anchor)
        .await
        .unwrap();
    let set_model = InboundEvent {
        body: InboundBody::Command(Command::SelectModel {
            model: "second-model".into(),
        }),
        ..dm("change-model", "two", "")
    };
    driver.push(set_model.clone()).await;
    applied(&gateway, "change-model", "command_complete").await;
    assert_eq!(
        ingress
            .invocation_outcome(set_model.id, set_model.room, set_model.sender)
            .await
            .unwrap(),
        Some(InvocationOutcome::Applied)
    );
    driver.push(first).await;
    driver.push(second).await;
    assert_eq!(driver.sent().await.attempt.payload.room.chat, "one");
    assert_eq!(driver.sent().await.attempt.payload.room.chat, "two");
    let final_state = status(&gateway, |s| {
        input(s, "second").is_some_and(|row| row["state"] == "done")
    })
    .await;
    let first = input(&final_state, "first").unwrap();
    let second = input(&final_state, "second").unwrap();
    assert_eq!(first["session"], second["session"]);
    assert!(first["session"].is_string());
    assert_ne!(first["route"], second["route"]);
    assert_eq!(model.requests()[0]["model"], "fixture");
    assert_eq!(model.requests()[1]["model"], "second-model");
    let id = gateway
        .send_messaging_notification(&member, "manual-1", "notice".into())
        .unwrap();
    assert_eq!(
        id,
        gateway
            .send_messaging_notification(&member, "manual-1", "notice".into())
            .unwrap()
    );
    assert!(
        gateway
            .send_messaging_notification(&member, "manual-1", "changed".into())
            .is_err()
    );
    assert_eq!(driver.sent().await.attempt.delivery_id, id);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn deferred_native_result_survives_restart_and_is_claimed_once() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let driver = Driver::new(Transport::Push, "result-actions", false);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    let original = dm("question", "one", "steer wait");
    driver.push(original.clone()).await;
    wait_for_model(&model, "steer wait").await;
    let action = ingress
        .defer_result(
            original.id.clone(),
            original.room.clone(),
            original.sender.clone(),
            expires(),
        )
        .await
        .unwrap();
    model.state.steer_release.add_permits(1);
    status(&gateway, |s| {
        input(s, "question").is_some_and(|row| row["state"] == "done")
    })
    .await;
    assert!(driver.sent.try_recv().is_err());
    gateway.shutdown().await.unwrap();

    let driver = Driver::new(Transport::Push, "result-actions", false);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    let fetch = |id, chat| InboundEvent {
        body: InboundBody::Command(Command::FetchResult {
            action: action.id.clone(),
        }),
        ..dm(id, chat, "")
    };
    let wrong = fetch("wrong", "two");
    driver.push(wrong.clone()).await;
    applied(&gateway, "wrong", "command_complete").await;
    assert_eq!(
        ingress
            .invocation_outcome(wrong.id, wrong.room, wrong.sender)
            .await
            .unwrap(),
        Some(InvocationOutcome::Forbidden)
    );
    let correct = fetch("retrieve", "one");
    driver.push(correct.clone()).await;
    applied(&gateway, "retrieve", "command_complete").await;
    let receipt = ingress
        .invocation_outcome(
            correct.id.clone(),
            correct.room.clone(),
            correct.sender.clone(),
        )
        .await
        .unwrap();
    assert!(matches!(
        receipt,
        Some(InvocationOutcome::ResultReady { .. })
    ));
    assert!(driver.push(correct.clone()).await.repeated);
    assert!(
        ingress
            .release_result(
                action.id.clone(),
                correct.id.clone(),
                correct.room.clone(),
                correct.sender.clone()
            )
            .await
            .unwrap()
    );
    let sent = driver.sent().await;
    assert_eq!(
        receipt,
        Some(InvocationOutcome::ResultReady {
            delivery: sent.attempt.delivery_id
        })
    );
    assert!(
        !ingress
            .release_result(action.id, correct.id, correct.room, correct.sender)
            .await
            .unwrap()
    );
    assert_eq!(model.requests().len(), 1);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn wake_only_recovers_new_work_without_a_message_send() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let make = || {
        let mut driver = Driver::new(Transport::Push, "wake-only", false);
        driver.fixture.capabilities = Capabilities {
            wake: true,
            ..Default::default()
        };
        driver
    };
    let driver = make();
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    assert_eq!(
        ingress.wake("first".into()).await.unwrap_err(),
        GatewayError::TemporarilyUnavailable
    );
    gateway
        .set_messaging_wake_target(
            "fixture",
            WakeTarget {
                target: "inbox".into(),
                instructions: "steer wait".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        ingress.wake("first".into()).await.unwrap().disposition,
        Disposition::Accepted
    );
    wait_for_model(&model, "steer wait").await;
    assert!(ingress.wake("first".into()).await.unwrap().repeated);
    assert_eq!(
        ingress.wake("second".into()).await.unwrap().disposition,
        Disposition::Accepted
    );
    model.state.steer_release.add_permits(2);
    status(&gateway, |s| {
        ["first", "second"]
            .iter()
            .all(|id| input(s, id).is_some_and(|r| r["state"] == "done"))
    })
    .await;
    assert!(driver.sent.try_recv().is_err());
    assert!(
        ingress
            .drain_activity(16)
            .await
            .unwrap()
            .events
            .iter()
            .any(|e| e.status == WakeStatus::Complete)
    );
    gateway.shutdown().await.unwrap();
    let driver = make();
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy())])
        .await;
    let ingress = driver.ready().await;
    assert!(ingress.wake("second".into()).await.unwrap().repeated);
    assert_eq!(model.requests().len(), 2);
    assert!(!ingress.capabilities().text_delivery);
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn prompt_decision_is_durable_and_reuse_checks_the_current_route_policy() {
    let root = TempDir::new().unwrap();
    let model = Model::new().await;
    let mut policy = policy();
    policy.direct_tools = true;
    policy.scope.direct.turn.approval = Some(ApprovalPolicy::Manual);
    policy.scope.direct.turn.approval_reuse = Some(ApprovalReuse::Persistent);
    let driver = Driver::new(Transport::Push, "approval-reuse", true);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy.clone())])
        .await;
    driver.ready().await;
    driver.push(dm("first", "one", "approval")).await;
    let prompt = driver.sent().await;
    let id = prompt_id(&prompt.attempt).to_owned();
    assert!(matches!(
        prompt.attempt.payload.body,
        DeliveryBody::Prompt {
            prompt: Prompt::Approval {
                allow_session: true,
                allow_persistent: true,
                ..
            },
            ..
        }
    ));
    prompt.finish(confirmed("approval-card"));
    let approve = control(
        "approve",
        "one",
        Control::Respond {
            action_id: id,
            response: Response::ApprovePersistently,
        },
    );
    driver.push(approve.clone()).await;
    applied(&gateway, "approve", "applied").await;
    let first = driver.sent().await;
    let delivery_id = first.attempt.delivery_id.clone();
    first.finish(confirmed("first-reply"));
    status(&gateway, |state| {
        delivery(state, &delivery_id, 0).is_some_and(|row| row["part_state"] == "sent")
    }).await;
    gateway.shutdown().await.unwrap();

    let driver = Driver::new(Transport::Push, "approval-reuse", true);
    let gateway = model
        .gateway(root.path(), vec![driver.config(policy.clone())])
        .await;
    driver.ready().await;
    assert!(driver.push(approve).await.repeated);
    driver.push(dm("second", "one", "approval")).await;
    let second = driver.sent().await;
    assert!(matches!(
        second.attempt.payload.body,
        DeliveryBody::Reply(_)
    ));
    second.finish(confirmed("second-reply"));
    policy.scope.direct.turn.approval_reuse = Some(ApprovalReuse::Session);
    gateway.set_adapter_policy("fixture", policy).await.unwrap();
    driver.push(dm("third", "one", "approval")).await;
    let prompt = driver.sent().await;
    let id = prompt_id(&prompt.attempt).to_owned();
    assert!(matches!(
        prompt.attempt.payload.body,
        DeliveryBody::Prompt {
            prompt: Prompt::Approval {
                allow_persistent: false,
                ..
            },
            ..
        }
    ));
    prompt.finish(confirmed("next-prompt"));
    driver
        .push(control(
            "too-broad",
            "one",
            Control::Respond {
                action_id: id.clone(),
                response: Response::ApprovePersistently,
            },
        ))
        .await;
    applied(&gateway, "too-broad", "forbidden").await;
    driver
        .push(control(
            "deny",
            "one",
            Control::Respond {
                action_id: id,
                response: Response::Deny,
            },
        ))
        .await;
    applied(&gateway, "deny", "applied").await;
    driver.sent().await.finish(confirmed("third-reply"));
    gateway.shutdown().await.unwrap();
}
