//! Native sign-in to an OAuth-mode remote or Hermes Cloud gateway
//! (electron/native-oauth.ts, hermes_cli/gateway_connections_oauth.py): PKCE
//! with a loopback redirect to the gateway's `/auth/native/authorize`, the code
//! exchanged at `/auth/native/token`, refreshed at `/auth/native/refresh`. The
//! access token is the bearer for `/api/auth/ws-ticket`.
use super::credential_files;
use hermes_engine::{mcp_oauth::{random, s256, wait_for_code}, secret};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io, path::{Path, PathBuf}, time::Duration};

const SKEW: i64 = 60;
const EXPIRED: &str = "The gateway session expired. Sign in again in Settings › Gateways.";
type SignIns = BTreeMap<String, Value>;

fn store(root: &Path) -> PathBuf { root.join("gateway-oauth.json") }

enum ReadError {
    File(io::Error),
    Unreadable(String),
}

impl ReadError {
    fn message(self) -> String {
        match self {
            Self::File(error) => error.to_string(),
            Self::Unreadable(error) => format!(
                "Saved gateway sign-ins could not be read. {error}. If the storage key is lost, reset the saved sign-ins in Settings › Gateways."
            ),
        }
    }
}

fn read(root: &Path) -> Result<SignIns, ReadError> {
    let bytes = match std::fs::read(store(root)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SignIns::new()),
        Err(error) => return Err(ReadError::File(error)),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ReadError::Unreadable("The sign-in file is damaged".into()))?;
    let decoded = secret::unseal(text.trim())
        .map_err(|error| ReadError::Unreadable(format!("{error:#}")))?;
    serde_json::from_str(&decoded)
        .map_err(|_| ReadError::Unreadable("The sign-in file is damaged".into()))
}

fn write(root: &Path, all: &SignIns) -> Result<(), String> {
    let text = serde_json::to_string(all).map_err(|e| e.to_string())?;
    let sealed = secret::seal(root, &text).map_err(|e| format!("{e:#}"))?;
    credential_files::replace(&store(root), sealed.as_bytes())
}

fn key(base: &str) -> String { base.trim().trim_end_matches('/').to_owned() }
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().timeout(Duration::from_secs(15)).redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string())
}
fn now() -> i64 { chrono::Utc::now().timestamp() }

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) enum Status {
    #[default]
    SignedOut,
    SignedIn,
    ResetAvailable,
}

pub(super) fn status(root: &Path, base: &str) -> Status {
    match read(root) {
        Ok(all) if all.get(&key(base)).is_some_and(|t| t["access_token"].as_str().is_some_and(|a| !a.is_empty())) => Status::SignedIn,
        Err(ReadError::Unreadable(_)) => Status::ResetAvailable,
        _ => Status::SignedOut,
    }
}

pub(super) fn sign_out(root: &Path, base: &str) -> Result<(), String> {
    let _lock = credential_files::lock(&store(root))?;
    let mut all = read(root).map_err(ReadError::message)?;
    all.remove(&key(base));
    write(root, &all)
}

pub(super) fn reset_unreadable(root: &Path) -> Result<(), String> {
    let path = store(root);
    let _lock = credential_files::lock(&path)?;
    match read(root) {
        Err(ReadError::Unreadable(_)) => credential_files::remove(&path),
        Err(error) => Err(error.message()),
        Ok(_) => Err("Saved gateway sign-ins can be read now. Sign out of one gateway at a time.".into()),
    }
}

/// Whether the gateway offers the native PKCE flow (`/api/status` → auth_flows).
pub(super) async fn supports_native(base: &str) -> Result<bool, String> {
    let response = client()?.get(format!("{}/api/status", key(base))).send().await
        .map_err(|_| "The gateway is not answering. A Hermes Cloud agent may be waking up — try again in a minute.".to_owned())?;
    Ok(response.json::<Value>().await.ok().and_then(|s| s["auth_flows"].as_array().map(|f| f.iter().any(|v| v == "native_pkce"))).unwrap_or(false))
}

/// Browser sign-in; returns once the gateway has issued tokens.
pub(super) async fn sign_in(root: PathBuf, base: String) -> Result<(), String> { sign_in_with(root, base, super::open_url).await }
async fn sign_in_with(root: PathBuf, base: String, open: impl Fn(&str) -> Result<(), String>) -> Result<(), String> {
    read(&root).map_err(ReadError::message)?;
    let base = key(&base);
    let url = reqwest::Url::parse(&base).map_err(|_| "Invalid gateway URL".to_owned())?;
    if url.scheme() != "https" && !matches!(url.host_str(), Some("127.0.0.1" | "localhost")) { return Err("Gateway sign-in needs an HTTPS address.".into()); }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr().map_err(|e| e.to_string())?.port());
    let (verifier, state) = (random(32), random(16));
    let mut authorize = reqwest::Url::parse(&format!("{base}/auth/native/authorize")).map_err(|e| e.to_string())?;
    authorize.query_pairs_mut().append_pair("code_challenge", &s256(&verifier)).append_pair("code_challenge_method", "S256").append_pair("redirect_uri", &redirect).append_pair("state", &state);
    open(authorize.as_str())?;
    let expected = state.clone();
    let code = tokio::task::spawn_blocking(move || wait_for_code(listener, &expected)).await.map_err(|e| e.to_string())?.map_err(|e| format!("{e:#}"))?;
    let response = client()?.post(format!("{base}/auth/native/token")).json(&json!({"code": code, "code_verifier": verifier})).send().await.map_err(|e| format!("Could not reach {base}: {e}"))?;
    if !response.status().is_success() { return Err(format!("{base} refused the sign-in code ({}).", response.status())); }
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    let tokens = parse(&body)?;
    let _lock = credential_files::lock(&store(&root))?;
    let mut all = read(&root).map_err(ReadError::message)?;
    all.insert(base, tokens);
    write(&root, &all)
}
fn parse(body: &Value) -> Result<Value, String> {
    let access = body["access_token"].as_str().filter(|a| !a.is_empty()).ok_or("The gateway's token response has no access token.")?;
    Ok(json!({"access_token": access, "refresh_token": body["refresh_token"].as_str().unwrap_or_default(), "expires_at": body["expires_at"].as_f64().map(|f| f as i64).unwrap_or(0),
        "provider": body["provider"].as_str().unwrap_or_default()}))
}

fn stored_tokens(root: &Path, base: &str) -> Result<Value, String> {
    read(root).map_err(ReadError::message)?.remove(base)
        .ok_or_else(|| "Sign in to this gateway in Settings › Gateways.".into())
}

fn needs_refresh(tokens: &Value) -> bool {
    tokens["expires_at"].as_i64().unwrap_or(0) <= now() + SKEW
}

fn get_access(tokens: &Value) -> Result<String, String> {
    tokens["access_token"].as_str().map(str::to_owned).ok_or_else(|| "No access token".into())
}

fn finish_refresh(root: &Path, base: &str, previous: &Value, fresh: Option<Value>) -> Result<(), String> {
    let _lock = credential_files::lock(&store(root))?;
    let mut all = read(root).map_err(ReadError::message)?;
    if all.get(base) != Some(previous) {
        return Err("The gateway sign-in changed while refreshing. Try connecting again.".into());
    }
    if let Some(tokens) = fresh {
        all.insert(base.to_owned(), tokens);
    } else {
        all.remove(base);
    }
    write(root, &all)
}

/// A current access token, refreshed through `/auth/native/refresh` when near expiry.
pub(super) async fn access_token(root: &Path, base: &str) -> Result<String, String> {
    let base = key(base);
    let tokens = stored_tokens(root, &base)?;
    if !needs_refresh(&tokens) {
        return get_access(&tokens);
    }

    // The server can rotate refresh tokens. Let concurrent callers reuse the first renewal,
    // without holding the store's write lock during the HTTP request.
    let refresh_path = root.join("gateway-oauth-refresh");
    let _refresh_lock = tokio::task::spawn_blocking(move || {
        credential_files::lock_for(&refresh_path, Duration::from_secs(20))
    }).await.map_err(|e| e.to_string())??;
    let tokens = stored_tokens(root, &base)?;
    if !needs_refresh(&tokens) {
        return get_access(&tokens);
    }
    let refresh = tokens["refresh_token"].as_str().filter(|r| !r.is_empty()).ok_or(EXPIRED)?;
    let response = client()?.post(format!("{base}/auth/native/refresh")).json(&json!({"refresh_token": refresh, "provider": tokens["provider"]})).send().await.map_err(|e| e.to_string())?;
    if response.status().as_u16() == 401 {
        finish_refresh(root, &base, &tokens, None)?;
        return Err(EXPIRED.into());
    }
    if !response.status().is_success() { return Err(format!("Could not renew the gateway session ({}).", response.status())); }
    let mut fresh = parse(&response.json::<Value>().await.map_err(|e| e.to_string())?)?;
    if fresh["refresh_token"].as_str().is_some_and(str::is_empty) { fresh["refresh_token"] = tokens["refresh_token"].clone(); }
    let access = get_access(&fresh)?;
    finish_refresh(root, &base, &tokens, Some(fresh))?;
    Ok(access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::{get, post}};
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    #[tokio::test]
    async fn native_sign_in_then_refresh_supplies_the_ws_ticket_bearer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let refreshes = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&refreshes);
        let app = Router::new()
            .route("/api/status", get(|| async { Json(json!({"auth_flows":["native_pkce"]})) }))
            .route("/auth/native/token", post(|Json(b): Json<Value>| async move { assert_eq!(b["code"], "c0de"); assert!(b["code_verifier"].as_str().unwrap().len() > 40);
                Json(json!({"access_token":"a1","refresh_token":"r1","expires_at":0,"provider":"nous"})) }))
            .route("/auth/native/refresh", post(move |Json(b): Json<Value>| {
                count.fetch_add(1, Ordering::Relaxed);
                async move {
                    assert_eq!(b["refresh_token"], "r1");
                    Json(json!({"access_token":"a2","expires_at": chrono::Utc::now().timestamp() + 3600}))
                }
            }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let root = tempfile::tempdir().unwrap();
        assert!(supports_native(&base).await.unwrap());
        std::fs::write(store(root.path()), "unreadable sign-in file").unwrap();
        assert_eq!(status(root.path(), &base), Status::ResetAvailable);
        assert!(sign_out(root.path(), &base).is_err());
        reset_unreadable(root.path()).unwrap();
        let other = "https://other-gateway.example";
        let legacy = SignIns::from([(other.to_owned(), json!({"access_token":"other-token"}))]);
        std::fs::write(store(root.path()), serde_json::to_vec(&legacy).unwrap()).unwrap();

        let open = |authorize: &str| {
            let url = reqwest::Url::parse(authorize).unwrap();
            let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).unwrap().1.into_owned();
            assert_eq!(q("code_challenge_method"), "S256");
            let callback = reqwest::Url::parse(&format!("{}?code=c0de&state={}", q("redirect_uri"), q("state"))).unwrap();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut s = std::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap())).unwrap();
                write!(s, "GET {}?{} HTTP/1.1\r\nHost: x\r\n\r\n", callback.path(), callback.query().unwrap()).unwrap();
                let mut out = String::new(); let _ = s.read_to_string(&mut out);
            });
            Ok(())
        };
        sign_in_with(root.path().to_owned(), base.clone(), open).await.unwrap();
        assert_eq!(status(root.path(), &base), Status::SignedIn);
        assert!(reset_unreadable(root.path()).is_err(), "readable credentials are never reset");
        let (first, second) = tokio::join!(access_token(root.path(), &base), access_token(root.path(), &base));
        assert_eq!(first.unwrap(), "a2", "an expired token is renewed");
        assert_eq!(second.unwrap(), "a2", "concurrent callers share the renewal");
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
        assert_eq!(access_token(root.path(), &base).await.unwrap(), "a2", "and reused while fresh");
        let previous = stored_tokens(root.path(), &base).unwrap();
        sign_out(root.path(), &base).unwrap();
        assert!(finish_refresh(root.path(), &base, &previous, Some(previous.clone())).is_err());
        assert_eq!(status(root.path(), &base), Status::SignedOut);
        assert_eq!(status(root.path(), other), Status::SignedIn);
    }
}
