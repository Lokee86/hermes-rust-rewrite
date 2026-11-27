//! Local slash command for reviewing and approving the active PHY Identity.

use super::*;
use hermes_engine::settings::ProfileSettings;

impl Shared {
    pub(super) fn ego_available(&self) -> bool {
        stock_plugins::has_memory_plugin()
            && ProfileSettings::load(self.library.root()).is_ok_and(|settings| {
                settings.memory_ego_enabled
                    && settings.memory_provider.resolve(self.memory.active())
                        == hermes_engine::settings::MemoryProvider::Reliquary
            })
    }

    pub(super) async fn identity_command(
        self: &Arc<Self>,
        session: Arc<LiveSession>,
        peer: &Peer,
        arguments: &str,
    ) -> RpcResult {
        if !self.ego_available() {
            return Err(RpcError::new(
                4004,
                "ego_unavailable",
                "Enable Reliquary Ego for this profile.",
            ));
        }
        let ego = self
            .selected_memory()
            .await
            .ego(&self.library, &session)
            .map_err(RpcError::internal)?
            .ok_or_else(|| {
                RpcError::new(4004, "ego_unavailable", "The active PHY is unavailable.")
            })?;
        let text = arguments.trim_start();
        if text.trim().is_empty() {
            let current = tokio::task::spawn_blocking(move || ego.identity())
                .await
                .map_err(RpcError::internal)?
                .map_err(RpcError::internal)?;
            return Ok(json!({"type":"exec","output":current.unwrap_or_else(||
                "There is no active Ego Identity. Use /identity followed by the exact text to review a draft.".into())}));
        }
        let provider_key = lock(&session.config).api_key.clone();
        if text.len() > 8192
            || hermes_engine::memory_capture::contains_inline_data(text)
            || !provider_key.is_empty() && text.contains(&provider_key)
            || hermes_engine::native_memory::redact_text(text) != text
        {
            return Err(RpcError::invalid(
                "Enter a bounded Identity draft without credentials or attachments.",
            ));
        }
        let text = text.to_owned();
        let review = tokio::task::spawn_blocking(move || ego.prepare_identity(text))
            .await
            .map_err(RpcError::internal)?
            .map_err(RpcError::internal)?;
        let id = new_id();
        let timeout = CancellationToken::new();
        let payload = {
            let mut view = lock(&session.view);
            if view.running || !view.approvals.is_empty() {
                return Err(RpcError::new(
                    4001,
                    "approval_busy",
                    "Finish the active turn or approval first.",
                ));
            }
            let payload = json!({
                "kind":"approval","prompt_id":id,"request_id":id,"execution_generation":view.generation,
                "choices":["once","deny"],"allow_session":false,"allow_permanent":false,
                "tool_name":"ego_identity","description":"Approve this exact active PHY Identity?",
                "command":review.review["text"],"identity_draft":review.review,"timeout_s":300
            });
            let stop = peer.stop.clone();
            let shutdown = self.shutdown.clone();
            let apply = review.apply;
            view.approvals.insert(
                id.clone(),
                Approval {
                    payload: payload.clone(),
                    timeout: timeout.clone(),
                    action: ApprovalAction::Identity {
                        peer: peer.id.clone(),
                        apply: Box::new(move || {
                            ensure!(
                                !stop.is_cancelled() && !shutdown.is_cancelled(),
                                "Identity review expired"
                            );
                            apply()
                        }),
                    },
                },
            );
            payload
        };
        self.emit(&session, "approval.request", payload);
        let shared = self.clone();
        let disconnect = peer.stop.clone();
        self.gateway_tasks.spawn(async move {
            tokio::select! {
                _ = timeout.cancelled() => return,
                _ = disconnect.cancelled() => {},
                _ = shared.shutdown.cancelled() => {},
                _ = tokio::time::sleep(Duration::from_secs(300)) => {},
            }
            if lock(&session.view).approvals.remove(&id).is_some() {
                shared.emit(
                    &session,
                    "approval.settled",
                    json!({"prompt_id":id,"request_id":id,"status":"expired"}),
                );
            }
        });
        Ok(json!({"type":"exec","output":"Review the active PHY Identity draft in Approvals."}))
    }
}
