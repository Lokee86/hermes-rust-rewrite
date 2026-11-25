//! Catalog controls for the private profile client. The native library owns the on-disk journal.
use super::*;

impl Shared {
    pub(super) async fn catalog_edit(
        self: &Arc<Self>,
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
        if method == "catalog.bot.delete" {
            let name = required_string(params, "name")?.to_owned();
            return self.delete_bot(&name).await;
        }
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
            for id in &archived {
                self.fence_python_source(id, python_memory::TerminalReason::Archive).await.map_err(RpcError::internal)?;
                self.memory_source_terminal(id).await.map_err(RpcError::internal)?;
            }
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
                // Recover the terminal fence if an earlier owner stopped between
                // committing the archive and handing it off to memory.
                if archived == Some(false) && self.library.session(&id).is_ok_and(|s| s.archived) {
                    self.fence_python_source(&id, python_memory::TerminalReason::Archive).await.map_err(RpcError::internal)?;
                }
                let summary = tokio::task::spawn_blocking(move || {
                    library.set_session_flags(&selected, archived, pinned)
                })
                .await
                .map_err(RpcError::internal)?
                .map_err(RpcError::internal)?;
                if archived == Some(true) {
                    self.fence_python_source(&id, python_memory::TerminalReason::Archive).await.map_err(RpcError::internal)?;
                    self.memory_source_terminal(&id).await.map_err(RpcError::internal)?;
                }
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
                if self.library.session(&id).is_ok_and(|s| s.archived) {
                    // Save the destinations before deleting the record that
                    // identifies which providers received its history.
                    self.fence_python_source(&id, python_memory::TerminalReason::Archive).await.map_err(RpcError::internal)?;
                }
                tokio::task::spawn_blocking(move || library.delete_archived_if_present(&selected))
                    .await
                    .map_err(RpcError::internal)?
                    .map_err(RpcError::internal)?;
                self.fence_python_source(&id, python_memory::TerminalReason::Delete).await.map_err(RpcError::internal)?;
                self.memory_source_terminal(&id).await.map_err(RpcError::internal)?;
                Ok(json!({"deleted":true,"session_id":id}))
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod bot_deletion_tests {
    use super::*;

    #[tokio::test]
    async fn private_bot_delete_fences_active_target_and_preserves_other_bots() {
        let home = tempfile::tempdir().unwrap();
        let config = hermes_engine::Config {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "fixture".into(),
            api_key: String::new(),
        };
        let gateway = Gateway::start(GatewayConfig::new(home.path(), config)).await.unwrap();
        let ada = gateway.shared.library.create_bot("Ada", "", "", "🤖").unwrap();
        let grace = gateway.shared.library.create_bot("Grace", "", "", "🤖").unwrap();
        let chat = gateway.shared.library.bot_chat(&ada.name).unwrap();
        let live = gateway.shared.session(&chat.id).await.unwrap();
        lock(&live.view).running = true;
        let (tx, _rx) = mpsc::channel(8);
        let peer = Peer { id: "bot-delete-test".into(), tx, stop: CancellationToken::new(), canonical: true, local_native: true };

        let refused = gateway.shared.dispatch("catalog.bot.delete", &json!({"name":ada.name}), &peer).await;
        assert!(refused.is_err());
        assert!(gateway.shared.library.bots().unwrap().iter().any(|bot| bot.name == ada.name));
        assert!(!gateway.shared.library.session(&chat.id).unwrap().archived);

        lock(&live.view).running = false;
        let removed = gateway.shared.dispatch("catalog.bot.delete", &json!({"name":ada.name}), &peer).await.unwrap();
        assert_eq!(removed["deleted"], true);
        assert!(gateway.shared.library.bots().unwrap().iter().all(|bot| bot.name != ada.name));
        assert!(gateway.shared.library.bots().unwrap().iter().any(|bot| bot.name == grace.name));
        let archived = gateway.shared.library.session(&chat.id).unwrap();
        assert!(archived.archived);
        assert!(archived.bot.is_none());
        assert!(gateway.shared.library.bot_profile(&ada.name).unwrap().exists() == false);
        drop(live);
        gateway.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bot_delete_requires_private_local_client() {
        let home = tempfile::tempdir().unwrap();
        let config = hermes_engine::Config {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "fixture".into(),
            api_key: String::new(),
        };
        let gateway = Gateway::start(GatewayConfig::new(home.path(), config)).await.unwrap();
        let bot = gateway.shared.library.create_bot("Ada", "", "", "🤖").unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let mut peer = Peer { id: "remote-delete-test".into(), tx, stop: CancellationToken::new(), canonical: true, local_native: false };
        assert!(gateway.shared.dispatch("catalog.bot.delete", &json!({"name":bot.name}), &peer).await.is_err());
        assert!(gateway.shared.library.bots().unwrap().iter().any(|candidate| candidate.name == bot.name));
        peer.local_native = true;
        gateway.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_bot_detach_preserves_profile_data_without_plugin_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let config = hermes_engine::Config {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "fixture".into(),
            api_key: String::new(),
        };
        let gateway = Gateway::start(GatewayConfig::new(home.path(), config)).await.unwrap();
        let library = &gateway.shared.library;
        let bot = library.create_bot("Legacy Helper", "", "", "").unwrap();
        let chat = library.bot_chat(&bot.name).unwrap();
        let profile = library.bot_profile(&bot.name).unwrap();
        std::fs::remove_file(profile.join(".hermes-bot-owner.json")).unwrap();
        let roster_path = library.root().join("bots.json");
        let mut roster: Value = serde_json::from_slice(&std::fs::read(&roster_path).unwrap()).unwrap();
        roster.as_object_mut().unwrap().remove("profile_owners");
        std::fs::write(roster_path, serde_json::to_vec(&roster).unwrap()).unwrap();
        let data = profile.join("plugin-data");
        std::fs::create_dir(&data).unwrap();
        let locator = data.join("reliquary-storage.json");
        // An invalid locator would fail any attempted plugin purge.
        std::fs::write(&locator, b"legacy data must be retained").unwrap();
        std::fs::write(profile.join("important.txt"), b"ordinary profile data").unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let peer = Peer { id: "legacy-delete-test".into(), tx, stop: CancellationToken::new(), canonical: true, local_native: true };

        let reply = gateway.shared.dispatch("catalog.bot.delete", &json!({"name":bot.name}), &peer).await.unwrap();
        assert_eq!(reply["deleted"], true);
        assert_eq!(reply["retained_profile"], json!(profile));
        assert_eq!(std::fs::read(locator).unwrap(), b"legacy data must be retained");
        assert_eq!(std::fs::read(profile.join("important.txt")).unwrap(), b"ordinary profile data");
        assert!(library.bots().unwrap().is_empty());
        assert!(library.session(&chat.id).unwrap().archived);
        assert!(library.session(&chat.id).unwrap().bot.is_none());
        assert!(!library.profiles().unwrap().into_iter().find(|entry| entry.name == bot.name).unwrap().is_bot);
        gateway.shutdown().await.unwrap();
    }
}

impl Shared {
    async fn delete_bot(self: &Arc<Self>, name: &str) -> RpcResult {
        let bot_fence = self.bot_write_guard(name).await;
        let library = self.library.clone();
        let bots = library.bots().map_err(RpcError::internal)?;
        if !bots.iter().any(|bot| bot.name == name) {
            return Err(RpcError::new(4004, "not_found", "This bot no longer exists"));
        }
        let expected = library.bot_profile(name).map_err(RpcError::internal)?;
        let lock_path = expected.clone();
        let bot_name = name.to_owned();
        let (rename_fence, install_fence, already_owned) =
            tokio::task::spawn_blocking(move || -> Result<_> {
                let rename_fence = owner::lock_profile_renames(&lock_path)?;
                let home = library.canonical_bot_profile(&bot_name)?;
                let install_fence = hermes_engine::plugins::lock_profile_plugin_deletion(&lock_path)?;
                // Close the owner file before removal. The rename fence still
                // prevents a separate Gateway from starting in the bot's home.
                let already_owned = match &home {
                    Some(path) => owner::try_local_owner_lock(path)?.is_none(),
                    None => false,
                };
                Ok((rename_fence, install_fence, already_owned))
            })
            .await
            .map_err(RpcError::internal)?
            .map_err(RpcError::internal)?;
        if already_owned {
            return Err(RpcError::new(4001, "profile_active", "Close this bot's separate Gateway before deleting it"));
        }

        let affected_groups: std::collections::HashSet<_> = self.library.groups()
            .map_err(RpcError::internal)?
            .into_iter()
            .filter(|group| group.members.iter().any(|member| member == name))
            .map(|group| format!("group:{}", group.id))
            .collect();
        {
            let sessions = self.sessions.lock().await;
            for session in sessions.values() {
                let summary = lock(&session.summary).clone();
                let direct_target = summary.bot.as_deref() == Some(name);
                let view = lock(&session.view);
                let target_receipts = || view.authority.receipts.iter().filter(|receipt| {
                    receipt.group.as_ref().is_some_and(|member| member.name == name)
                });
                let group_target = summary.bot.as_ref().is_some_and(|key| affected_groups.contains(key))
                    || target_receipts().next().is_some();
                let active_receipt = if direct_target {
                    view.authority.receipts.iter().any(|receipt| receipt.status != "terminal")
                } else {
                    target_receipts().any(|receipt| receipt.status != "terminal")
                };
                if active_receipt || (view.running && (direct_target || group_target)) {
                    return Err(RpcError::new(4001, "session_busy", "Stop the bot's active response before deleting it"));
                }
                if group_target && session.member_engines.try_lock().is_err() {
                    return Err(RpcError::new(4001, "session_busy", "Wait for the group chat to finish before deleting this bot"));
                }
            }
        }

        let runtime = self.other_plugin_profiles.lock().await.get(&expected).cloned();
        if let Some(runtime) = runtime {
            runtime.close().await.map_err(RpcError::internal)?;
            self.other_plugin_profiles.lock().await.remove(&expected);
        }

        // Remove cached direct sessions so no later request can revive a stale
        // engine after the profile has been removed. Keep group transcripts and
        // their other members; only drop this bot's member engine/control.
        let mut evicted = Vec::new();
        let mut groups_to_update = Vec::new();
        {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, session| {
                let summary = lock(&session.summary);
                let has_target_receipt = lock(&session.view).authority.receipts.iter()
                    .any(|receipt| receipt.group.as_ref().is_some_and(|member| member.name == name));
                if summary.bot.as_deref() == Some(name) {
                    evicted.push(session.clone());
                    false
                } else {
                    if summary.bot.as_ref().is_some_and(|key| affected_groups.contains(key)) || has_target_receipt {
                        groups_to_update.push(session.clone());
                    }
                    true
                }
            });
        }
        for session in evicted {
            if let Some(cancel) = lock(&session.view).cancel.take() { cancel.cancel(); }
            for peer in lock(&session.peers).values() { peer.stop.cancel(); }
        }
        for session in groups_to_update {
            if let Ok(mut members) = session.member_engines.try_lock() { members.remove(name); }
            lock(&session.member_controls).remove(name);
        }

        let library_for_delete = self.library.clone();
        let name_for_delete = name.to_owned();
        let outcome = tokio::task::spawn_blocking(move || {
            let _fences = (bot_fence, rename_fence, install_fence);
            stock_plugins::delete_bot(&library_for_delete, &name_for_delete)
        })
            .await.map_err(RpcError::internal)?.map_err(RpcError::internal)?;
        let mut reply = json!({"deleted":true,"name":name});
        if let Some(profile) = outcome.retained_profile {
            reply["retained_profile"] = json!(profile);
        }
        Ok(reply)
    }
}
