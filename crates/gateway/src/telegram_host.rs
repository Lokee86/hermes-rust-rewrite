use hermes_telegram::{Client, Identity, Store, Token, Transport};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

struct Active {
    bot: Identity,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    _lease: Arc<std::fs::File>,
}

pub(crate) struct Host {
    store: Store,
    transport: Option<Arc<dyn Transport>>,
    active: Mutex<Option<Active>>,
    shutdown: CancellationToken,
}

impl Host {
    pub(crate) fn new(root: &Path, shutdown: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            store: Store::new(root),
            transport: None,
            active: Mutex::new(None),
            shutdown,
        })
    }

    #[cfg(test)]
    fn with_transport(
        root: &Path,
        shutdown: CancellationToken,
        transport: Arc<dyn Transport>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: Store::new(root),
            transport: Some(transport),
            active: Mutex::new(None),
            shutdown,
        })
    }

    fn client(
        &self,
        token: Token,
        allowed: std::collections::BTreeSet<i64>,
    ) -> Result<Client, String> {
        match &self.transport {
            Some(transport) => Client::with_transport(token, allowed, transport.clone()),
            None => Client::new(token, allowed),
        }
        .map_err(|e| e.to_string())
    }

    pub(crate) fn restore(host: &Arc<Self>) {
        let host = host.clone();
        tokio::spawn(async move {
            loop {
                if host.shutdown.is_cancelled() {
                    return;
                }
                let settings = match host.load_settings().await {
                    Ok(Some(settings)) if settings.enabled => settings,
                    _ => return,
                };
                match host.connect_loaded(settings).await {
                    Ok(_) => return,
                    Err(_) => {
                        tokio::select! {
                            _ = host.shutdown.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        }
                    }
                }
            }
        });
    }

    async fn load_settings(&self) -> Result<Option<hermes_telegram::Settings>, String> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.load_settings())
            .await
            .map_err(|_| "Could not load saved Telegram settings".to_owned())?
    }

    pub(crate) async fn status(&self) -> Result<Value, String> {
        let settings = self.load_settings().await?;
        let active = self.active.lock().await;
        let connected = active.as_ref().is_some_and(|a| !a.task.is_finished());
        let bot = active
            .as_ref()
            .map(|a| a.bot.clone())
            .or_else(|| settings.as_ref().and_then(|s| s.bot.clone()));
        Ok(json!({
            "enabled": settings.as_ref().is_some_and(|s| s.enabled),
            "connected": connected,
            "bot": bot,
        }))
    }

    pub(crate) async fn connect(&self) -> Result<Value, String> {
        let settings = self.load_settings().await?.ok_or_else(|| {
            "Save a Telegram bot token and allowlist before connecting.".to_owned()
        })?;
        self.connect_loaded(settings).await
    }

    async fn connect_loaded(&self, settings: hermes_telegram::Settings) -> Result<Value, String> {
        let mut active = self.active.lock().await;
        if let Some(current) = active.as_ref().filter(|a| !a.task.is_finished())
            && settings.enabled
            && settings.bot.as_ref().map(|saved| saved.id) == Some(current.bot.id)
        {
            return Ok(json!({"connected":true,"enabled":true,"bot":current.bot}));
        }
        if let Some(previous) = active.take() {
            previous.cancel.cancel();
            let _ = previous.task.await;
        }
        if settings.version != 1 || settings.token.is_none() || settings.allowed.is_empty() {
            return Err(
                "Save a Telegram bot token and non-empty allowlist before connecting.".into(),
            );
        }
        let token = settings.token.clone().unwrap();
        let client = self.client(token.clone(), settings.allowed)?;
        let verified = client
            .verify(&self.shutdown)
            .await
            .map_err(|e| e.to_string())?;
        if settings
            .bot
            .as_ref()
            .is_some_and(|saved| saved.id != verified.id)
        {
            return Err(
                "The saved Telegram bot identity changed; review the setup before reconnecting."
                    .into(),
            );
        }
        let bot = verified;
        let store = self.store.clone();
        let credential = token.clone();
        let bot_for_store = bot.clone();
        let (inbox, lease) = tokio::task::spawn_blocking(move || {
            let lease = store.claim(bot_for_store.id)?;
            let inbox = store.load_inbox(&bot_for_store)?;
            store.remember_bot(&bot_for_store, &credential)?;
            Ok::<_, String>((inbox, lease))
        })
        .await
        .map_err(|_| "Could not open the native Telegram inbox".to_owned())??;

        let cancel = self.shutdown.child_token();
        let task = tokio::spawn(run_polling(
            self.store.clone(),
            client,
            bot.clone(),
            token,
            inbox.cursor,
            cancel.clone(),
            lease.clone(),
        ));
        *active = Some(Active {
            bot: bot.clone(),
            cancel,
            task,
            _lease: lease,
        });
        Ok(json!({"connected":true,"enabled":true,"bot":bot}))
    }

    pub(crate) async fn disconnect(&self) -> Result<Value, String> {
        let mut active = self.active.lock().await;
        let store = self.store.clone();
        let persisted = tokio::task::spawn_blocking(move || store.set_disabled())
            .await
            .map_err(|_| "Could not save Telegram disconnect state".to_owned())?;
        if let Some(connection) = active.take() {
            connection.cancel.cancel();
            connection
                .task
                .await
                .map_err(|_| "Telegram polling task stopped unexpectedly".to_owned())?;
        }
        persisted?;
        Ok(json!({"connected":false,"enabled":false}))
    }

    pub(crate) async fn send(&self, user_id: i64, text: String) -> Result<Value, String> {
        let active = self.active.lock().await;
        let connection = active
            .as_ref()
            .filter(|a| !a.task.is_finished())
            .ok_or_else(|| "Telegram is disconnected; enable it before replying.".to_owned())?;
        let bot = connection.bot.clone();
        let store = self.store.clone();
        let client = {
            let settings = self
                .load_settings()
                .await?
                .ok_or_else(|| "Telegram setup is unavailable".to_owned())?;
            let token = settings
                .token
                .clone()
                .ok_or_else(|| "Telegram credential is unavailable".to_owned())?;
            self.client(token, settings.allowed)?
        };
        let credential = client.credential().clone();
        let policy_store = store.clone();
        let policy_bot = bot.clone();
        tokio::task::spawn_blocking(move || {
            policy_store.authorize_reply(&policy_bot, &credential, user_id)
        })
        .await
        .map_err(|_| "Could not check the saved Telegram allowlist".to_owned())??;
        let receipt = client
            .send(user_id, &text, &self.shutdown)
            .await
            .map_err(|e| e.to_string())?;
        let store_sent = store.clone();
        let record_bot = bot.clone();
        let inbox = tokio::task::spawn_blocking(move || {
            store_sent.record_sent(&record_bot, user_id, text, receipt)
        })
        .await
        .map_err(|_| "Reply delivered, but the native inbox could not be saved".to_owned())??;
        Ok(json!({"sent":true,"message_id":inbox.messages.last().map(|m|m.message_id)}))
    }

    pub(crate) async fn shutdown(&self) {
        let mut active = self.active.lock().await;
        if let Some(connection) = active.take() {
            connection.cancel.cancel();
            let _ = connection.task.await;
        }
    }
}

async fn run_polling(
    store: Store,
    client: Client,
    bot: Identity,
    token: Token,
    mut cursor: i64,
    cancel: CancellationToken,
    _lease: Arc<std::fs::File>,
) {
    loop {
        let store_check = store.clone();
        let bot_check = bot.clone();
        let token_check = token.clone();
        let enabled = tokio::task::spawn_blocking(move || {
            let Some(settings) = store_check.load_settings()? else {
                return Ok(false);
            };
            Ok::<_, String>(
                settings.enabled
                    && settings.bot.as_ref().map(|saved| saved.id) == Some(bot_check.id)
                    && settings.token.as_ref() == Some(&token_check),
            )
        })
        .await;
        if !matches!(enabled, Ok(Ok(true))) {
            return;
        }
        let batch = tokio::select! {
            _ = cancel.cancelled() => return,
            result = client.poll(bot.id, cursor, &cancel) => result,
        };
        match batch {
            Ok(batch) => {
                let store_poll = store.clone();
                let bot_poll = bot.clone();
                let token_poll = token.clone();
                let next = batch.next_offset;
                let messages = batch.messages;
                let result = tokio::task::spawn_blocking(move || {
                    store_poll.commit_poll(&bot_poll, &token_poll, &messages, next)
                })
                .await;
                match result {
                    Ok(Ok((inbox, _received))) => cursor = inbox.cursor,
                    Ok(Err(_)) => return,
                    Err(_) => return,
                }
            }
            Err(error) if error.kind == hermes_telegram::ErrorKind::Retryable => {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(error.retry_after.unwrap_or(3).clamp(1,60))) => {}
                }
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_telegram::{Method, RequestFuture};
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeSet, VecDeque},
        sync::Mutex as StdMutex,
    };

    struct Fake {
        first_updates: StdMutex<VecDeque<Value>>,
        calls: StdMutex<Vec<String>>,
    }

    impl Fake {
        fn new(updates: Vec<Value>) -> Arc<Self> {
            Arc::new(Self {
                first_updates: StdMutex::new(updates.into()),
                calls: StdMutex::new(Vec::new()),
            })
        }

        fn call_count(&self, name: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == name)
                .count()
        }
    }

    impl Transport for Fake {
        fn request<'a>(
            &'a self,
            _token: &'a Token,
            method: Method,
            body: Value,
        ) -> RequestFuture<'a> {
            let name = match method {
                Method::GetMe => "getMe",
                Method::GetWebhookInfo => "getWebhookInfo",
                Method::GetUpdates => "getUpdates",
                Method::SendMessage => "sendMessage",
            };
            self.calls.lock().unwrap().push(name.to_owned());
            let response = match method {
                Method::GetMe => Ok(
                    json!({"id":123456,"is_bot":true,"username":"synthetic_bot","first_name":"Synthetic"}),
                ),
                Method::GetWebhookInfo => Ok(json!({"url":""})),
                Method::GetUpdates => {
                    if body["offset"].as_i64().unwrap_or_default() > 0 {
                        Ok(json!([]))
                    } else {
                        Ok(self
                            .first_updates
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| json!([])))
                    }
                }
                Method::SendMessage => Ok(json!({"message_id":77,"date":1234})),
            };
            let pause = matches!(method, Method::GetUpdates);
            Box::pin(async move {
                if pause {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                response
            })
        }
    }

    fn private_update(update_id: i64, user_id: i64, chat_id: i64, text: &str) -> Value {
        json!({"update_id":update_id,"message":{
            "message_id":update_id,"date":1234,
            "from":{"id":user_id,"is_bot":false,"username":"tester"},
            "chat":{"id":chat_id,"type":"private"},"text":text
        }})
    }

    fn saved_settings(root: &Path) -> Store {
        let store = Store::new(root);
        store
            .save_settings(&hermes_telegram::Settings {
                version: 1,
                token: Some(Token::parse("123456:abcdefghijklmnopqrstuvwxyz1234567890").unwrap()),
                allowed: BTreeSet::from([42]),
                bot: None,
                enabled: false,
            })
            .unwrap();
        store
    }

    async fn wait_for_inbox(store: &Store, bot: &Identity, cursor: i64) -> hermes_telegram::Inbox {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let store = store.clone();
                let bot = bot.clone();
                let inbox = tokio::task::spawn_blocking(move || store.load_inbox(&bot))
                    .await
                    .unwrap()
                    .unwrap();
                if inbox.cursor >= cursor {
                    return (*inbox).clone();
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn gateway_owns_polling_and_persists_explicit_disconnect_across_restart() {
        let root = tempfile::tempdir().unwrap();
        let store = saved_settings(root.path());
        let update = private_update(10, 42, 42, "hello from allowed user");
        let unallowed = private_update(11, 99, 99, "should be ignored");
        let group = private_update(12, 42, -10042, "group should be ignored");
        let fake = Fake::new(vec![json!([update, unallowed, group])]);
        let host = Host::with_transport(root.path(), CancellationToken::new(), fake.clone());

        let connected = host.connect().await.unwrap();
        assert_eq!(connected["enabled"], true);
        assert_eq!(connected["bot"]["id"], 123456);
        let bot: Identity = serde_json::from_value(connected["bot"].clone()).unwrap();
        let inbox = wait_for_inbox(&store, &bot, 13).await;
        assert_eq!(inbox.cursor, 13);
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].text, "hello from allowed user");
        assert!(fake.call_count("getUpdates") > 0);

        assert!(host.send(99, "blocked".into()).await.is_err());
        host.send(42, "manual reply".into()).await.unwrap();
        let inbox = wait_for_inbox(&store, &bot, 13).await;
        assert_eq!(inbox.messages.len(), 2);
        assert!(inbox.messages[1].outgoing);
        assert_eq!(inbox.messages[1].text, "manual reply");

        host.shutdown().await;
        assert!(store.load_settings().unwrap().unwrap().enabled);
        let resumed_fake = Fake::new(vec![]);
        let resumed =
            Host::with_transport(root.path(), CancellationToken::new(), resumed_fake.clone());
        Host::restore(&resumed);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if resumed_fake.call_count("getUpdates") > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .unwrap();
        let state = resumed.status().await.unwrap();
        assert_eq!(state["enabled"], true);
        assert_eq!(state["connected"], true);
        assert_eq!(resumed_fake.call_count("getMe"), 1);

        resumed.disconnect().await.unwrap();
        let settings = store.load_settings().unwrap().unwrap();
        assert!(!settings.enabled);
        let state = resumed.status().await.unwrap();
        assert_eq!(state["connected"], false);

        let disconnected_fake = Fake::new(vec![]);
        let after_disconnect = Host::with_transport(
            root.path(),
            CancellationToken::new(),
            disconnected_fake.clone(),
        );
        Host::restore(&after_disconnect);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(disconnected_fake.call_count("getMe"), 0);
        assert_eq!(disconnected_fake.call_count("getUpdates"), 0);
    }
}
