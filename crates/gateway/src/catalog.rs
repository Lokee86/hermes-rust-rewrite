//! Catalog controls for the private profile client. The native library owns the on-disk journal.
use super::*;

impl Shared {
    pub(super) async fn catalog_edit(
        &self,
        method: &str,
        params: &Value,
        peer: &Peer,
    ) -> RpcResult {
        if !peer.local_native {
            return Err(RpcError::new(
                4403,
                "permission_denied",
                "Catalog changes require the private profile client",
            ));
        }
        let _order = self.catalog_edits.lock().await;
        let library = self.library.clone();
        if method == "catalog.archive_inactive" {
            let days = params["days"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=36500).contains(n))
                .ok_or_else(|| RpcError::invalid("Invalid inactive session age"))?;
            let current = optional_string(params, "current_session_id")?.map(str::to_owned);
            // The application's cached writer for a detached idle session must not exempt it from archiving.
            self.sessions
                .lock()
                .await
                .retain(|_, s| Arc::strong_count(s) > 1 || !Self::detached_idle(s));
            let archived = tokio::task::spawn_blocking(move || {
                library.archive_inactive_ids(days, current.as_deref())
            })
            .await
            .map_err(RpcError::internal)?
            .map_err(RpcError::internal)?;
            for id in &archived { self.memory_source_terminal(id).await.map_err(RpcError::internal)?; }
            return Ok(json!({"archived":archived}));
        }
        let id = required_string(params, "session_id")?.to_owned();
        let selected = id.clone();
        match method {
            "catalog.session.set" => {
                let flag = |key| {
                    params
                        .get(key)
                        .map(|v| {
                            v.as_bool()
                                .ok_or_else(|| RpcError::invalid("Catalog flag must be a boolean"))
                        })
                        .transpose()
                };
                let archived = flag("archived")?;
                let pinned = flag("pinned")?;
                if archived.is_none() && pinned.is_none() {
                    return Err(RpcError::invalid("A changed catalog flag is required"));
                }
                let summary = tokio::task::spawn_blocking(move || {
                    library.set_session_flags(&selected, archived, pinned)
                })
                .await
                .map_err(RpcError::internal)?
                .map_err(RpcError::internal)?;
                if archived == Some(true) { self.memory_source_terminal(&id).await.map_err(RpcError::internal)?; }
                let live = self.sessions.lock().await.get(&id).cloned();
                if let Some(live) = live {
                    *lock(&live.summary) = summary.clone();
                    self.emit(&live, "session.info", self.info(&live));
                }
                Ok(json!({"session":summary}))
            }
            "catalog.session.delete" => {
                {
                    let mut sessions = self.sessions.lock().await;
                    if let Some(s) = sessions.get(&id)
                        && (Arc::strong_count(s) != 1 || !Self::detached_idle(s))
                    {
                        return Err(RpcError::new(
                            4001,
                            "session_busy",
                            "Close this conversation before deleting it",
                        ));
                    }
                    sessions.remove(&id);
                }
                tokio::task::spawn_blocking(move || library.delete_archived_if_present(&selected))
                    .await
                    .map_err(RpcError::internal)?
                    .map_err(RpcError::internal)?;
                self.memory_source_terminal(&id).await.map_err(RpcError::internal)?;
                Ok(json!({"deleted":true,"session_id":id}))
            }
            _ => unreachable!(),
        }
    }
}
