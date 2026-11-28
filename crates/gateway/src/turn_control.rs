use super::*;

impl Shared {
    pub(super) async fn native_turn_control(
        &self,
        method: &str,
        params: &Value,
        peer: &Peer,
    ) -> RpcResult {
        if !peer.canonical {
            return Err(RpcError::unsupported(
                "This turn control requires the native gateway protocol",
            ));
        }
        let session = self.attached(params, peer).await?;
        if method == "prompt.replace_queued" {
            return self.replace_queued(
                &session,
                required_string(params, "admission_id")?,
                required_string(params, "text")?,
            );
        }
        let generation = params["execution_generation"]
            .as_u64()
            .ok_or_else(|| RpcError::invalid("execution_generation is required"))?;
        match method {
            "clarify.respond" => self.answer_prompt(
                &session,
                required_string(params, "request_id")?,
                generation,
                &messaging::Response::Clarify(required_string(params, "answer")?.into()),
            ),
            "session.steer" | "session.redirect" => self.correct_turn(
                &session,
                generation,
                required_string(params, "text")?,
                method == "session.redirect",
                optional_string(params, "request_id")?,
            ),
            _ => Err(RpcError::unsupported("Unknown turn control")),
        }
    }

    pub(super) fn stop_active_turn(
        session: &LiveSession,
        view: &mut View,
        park: bool,
    ) -> Result<(), RpcError> {
        if park
            && view.running
            && view
                .authority
                .receipts
                .iter()
                .any(|receipt| receipt.status == "queued")
        {
            let mut next = view.authority.clone();
            for receipt in &mut next.receipts {
                if receipt.status == "queued" {
                    receipt.status = "parked".into();
                }
            }
            next.persist(&session.writer, None)
                .map_err(RpcError::internal)?;
            view.authority = next;
            view.revision += 1;
        }
        if let Some(cancel) = &view.cancel {
            cancel.cancel();
        }
        Ok(())
    }

    pub(super) fn interrupt_generation(&self, session: &LiveSession, generation: u64) -> RpcResult {
        let mut view = lock(&session.view);
        Self::check_generation(&view, generation)?;
        Self::stop_active_turn(session, &mut view, false)?;
        Ok(json!({"status":"interrupting"}))
    }

    pub(super) fn correct_turn(
        &self,
        session: &LiveSession,
        generation: u64,
        text: &str,
        redirect: bool,
        id: Option<&str>,
    ) -> RpcResult {
        if text.trim().is_empty() || text.len() > MAX_PROMPT_BYTES {
            return Err(RpcError::invalid(
                "Enter a correction within the prompt limit",
            ));
        }
        let mut view = lock(&session.view);
        Self::check_generation(&view, generation)?;
        let result = json!({"status":"accepted","execution_generation":generation});
        let receipt = if let Some(id) = id {
            if id.is_empty() || id.len() > 256 {
                return Err(RpcError::invalid(
                    "Correction identity must contain 1-256 bytes",
                ));
            }
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            hash.update(&[u8::from(redirect)]);
            hash.update(text.as_bytes());
            let digest = hash.finish();
            if let Some((_, old)) = view.correction_receipts.iter().find(|(key, _)| key == id) {
                return if old.as_ref() == digest.as_ref() {
                    Ok(result)
                } else {
                    Err(RpcError::invalid("Correction identity was reused"))
                };
            }
            Some((id.to_owned(), digest))
        } else {
            None
        };
        let sender = view
            .corrections
            .as_ref()
            .map(|(sender, _)| sender)
            .ok_or_else(|| RpcError::invalid("The turn is no longer accepting corrections"))?;
        let correction = if redirect {
            hermes_engine::TurnCorrection::Redirect(text.to_owned())
        } else {
            hermes_engine::TurnCorrection::Steer(text.to_owned())
        };
        sender.try_send(correction).map_err(|_| {
            RpcError::new(
                4001,
                "correction_unavailable",
                "The turn cannot accept another correction yet",
            )
        })?;
        if let Some(receipt) = receipt {
            view.correction_receipts.push_back(receipt);
            if view.correction_receipts.len() > 256 {
                view.correction_receipts.pop_front();
            }
        }
        Ok(result)
    }

    pub(super) fn replace_queued(
        &self,
        session: &LiveSession,
        admission: &str,
        text: &str,
    ) -> RpcResult {
        if text.trim().is_empty() || text.len() > MAX_PROMPT_BYTES {
            return Err(RpcError::invalid(
                "Enter replacement text within the prompt limit",
            ));
        }
        let sid = lock(&session.summary).id.clone();
        let result = {
            let mut view = lock(&session.view);
            let mut next = view.authority.clone();
            let receipt = next
                .receipts
                .iter_mut()
                .find(|receipt| receipt.id == admission)
                .ok_or_else(|| RpcError::new(4004, "not_found", "No such admission"))?;
            if !matches!(receipt.status.as_str(), "queued" | "parked") || receipt.group.is_some() {
                return Err(RpcError::fenced(
                    "already_claimed",
                    "Only queued single-session work can be edited",
                    &view,
                ));
            }
            receipt.text = text.to_owned();
            let result = receipt.value(&self.profile_id, &sid, self.authority_epoch, false);
            next.persist(&session.writer, None)
                .map_err(RpcError::internal)?;
            view.authority = next;
            view.revision += 1;
            result
        };
        self.emit(session, "session.info", self.info(session));
        Ok(result)
    }

    pub(super) fn answer_prompt(
        &self,
        session: &LiveSession,
        request: &str,
        generation: u64,
        response: &messaging::Response,
    ) -> RpcResult {
        self.answer_prompt_checked(session, request, generation, response, || Ok(()))
    }

    pub(super) fn answer_prompt_checked(
        &self,
        session: &LiveSession,
        request: &str,
        generation: u64,
        response: &messaging::Response,
        before: impl FnOnce() -> Result<(), RpcError>,
    ) -> RpcResult {
        let settled = {
            let mut view = lock(&session.view);
            Self::check_generation(&view, generation)?;
            let now = chrono::Utc::now().timestamp_millis();
            let live = match response {
                messaging::Response::Clarify(answer) => {
                    if answer.trim().is_empty() || answer.len() > MAX_PROMPT_BYTES {
                        return Err(RpcError::invalid("Enter a clarification answer within the prompt limit"));
                    }
                    view.clarifications.get(request).is_some_and(|ask| !ask.answer.is_closed() && ask.expires_ms > now)
                }
                _ => view.approvals.get(request).is_some_and(Approval::tool_is_live),
            };
            if !live {
                return Err(RpcError::new(4001, "prompt_stale", "This prompt is no longer waiting"));
            }
            before()?;
            match response {
                messaging::Response::Clarify(answer) => {
                    view.clarifications
                        .remove(request)
                        .is_some_and(|ask| ask.answer.try_send(answer.clone()).is_ok())
                }
                _ => Self::answer_approval(&mut view, request, !matches!(response, messaging::Response::Deny)),
            }
        };
        if !settled {
            return Err(RpcError::new(
                4001,
                "prompt_stale",
                "This prompt is no longer waiting",
            ));
        }
        let event = if matches!(response, messaging::Response::Clarify(_)) {
            "clarify.settled"
        } else {
            "approval.settled"
        };
        self.emit(
            session,
            event,
            json!({"request_id":request,"prompt_id":request,"status":"resolved"}),
        );
        Ok(json!({"status":"resolved"}))
    }

    pub(super) fn answer_approval(view: &mut View, request: &str, approve: bool) -> bool {
        if !view.approvals.get(request).is_some_and(Approval::tool_is_live) {
            return false;
        }
        let pending = view.approvals.remove(request).expect("live approval");
        pending.timeout.cancel();
        matches!(pending.action, ApprovalAction::Tool(answer) if answer.try_send(approve).is_ok())
    }

    fn check_generation(view: &View, generation: u64) -> Result<(), RpcError> {
        if !view.running || view.generation != generation {
            return Err(RpcError::fenced(
                "stale_generation",
                "This control belongs to a different turn",
                view,
            ));
        }
        Ok(())
    }
}
