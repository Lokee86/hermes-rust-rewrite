//! Authentication for the local owner and the explicitly published Webapp.
use super::*;
use axum::{
    extract::FromRequestParts,
    http::{HeaderValue, request::Parts, uri::Authority},
    response::{Html, Redirect},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use std::net::IpAddr;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Audience {
    Local,
    Remote,
}

pub(super) struct Access {
    pub audience: Audience,
    identity: Identity,
    pub revoked: Option<CancellationToken>,
}

#[derive(Clone)]
struct Identity {
    provider: &'static str,
    user: String,
    label: String,
    expires_at: u64,
}

impl Identity {
    fn value(&self) -> Value {
        json!({"user_id":self.user,"display_name":self.label,"email":"","org_id":"",
            "provider":self.provider,"expires_at":self.expires_at})
    }
}

impl FromRequestParts<Arc<Shared>> for Access {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        shared: &Arc<Shared>,
    ) -> std::result::Result<Self, Response> {
        let Some(ConnectInfo(peer)) = parts.extensions.get::<ConnectInfo<SocketAddr>>() else {
            return Err(StatusCode::FORBIDDEN.into_response());
        };
        shared.auth.authorize(shared, &parts.headers, *peer)
    }
}

#[derive(Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    dashboard: DashboardConfig,
    #[serde(default)]
    webapp: WebappConfig,
}
#[derive(Default, Deserialize)]
struct DashboardConfig {
    #[serde(default)]
    basic_auth: BasicConfig,
    #[serde(default)]
    session_token_auth: TokenConfig,
}
#[derive(Default, Deserialize)]
struct WebappConfig {
    public_url: Option<String>,
}
#[derive(Default, Deserialize)]
struct TokenConfig {
    enabled: Option<bool>,
}
#[derive(Default, Deserialize)]
struct BasicConfig {
    username: Option<String>,
    password: Option<String>,
    password_hash: Option<String>,
    session_ttl_seconds: Option<u64>,
}

struct Password {
    username: String,
    params: scrypt::Params,
    salt: Vec<u8>,
    derived: Vec<u8>,
    ttl: Duration,
}

impl Password {
    fn load(config: BasicConfig) -> Result<Option<Self>> {
        let username = setting("HERMES_DASHBOARD_BASIC_AUTH_USERNAME", config.username);
        let env_password = setting("HERMES_DASHBOARD_BASIC_AUTH_PASSWORD", None);
        let password_hash = setting(
            "HERMES_DASHBOARD_BASIC_AUTH_PASSWORD_HASH",
            config.password_hash,
        );
        let password = env_password.clone().or(config.password);
        let Some(username) = username.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        if password.is_none() && password_hash.is_none() {
            return Ok(None);
        }
        let ttl = setting("HERMES_DASHBOARD_BASIC_AUTH_TTL_SECONDS", None)
            .and_then(|s| s.parse().ok())
            .or(config.session_ttl_seconds)
            .unwrap_or(43_200)
            .clamp(60, 2_592_000);
        let (params, salt, derived) = if let Some(encoded) =
            password_hash.filter(|_| env_password.is_none())
        {
            let mut fields = encoded.split('$');
            let parse = |value: Option<&str>| value.and_then(|s| s.parse::<u32>().ok());
            let scheme = fields.next();
            let n = parse(fields.next());
            let r = parse(fields.next());
            let p = parse(fields.next());
            let salt = fields.next().and_then(|s| STANDARD.decode(s).ok());
            let dk = fields.next().and_then(|s| STANDARD.decode(s).ok());
            let (Some(n), Some(r), Some(p), Some(salt), Some(dk)) = (n, r, p, salt, dk) else {
                anyhow::bail!("Invalid dashboard.basic_auth.password_hash");
            };
            ensure!(
                scheme == Some("scrypt")
                    && fields.next().is_none()
                    && n.is_power_of_two()
                    && n >= 2
                    && r > 0
                    && p > 0
                    && u64::from(n) * u64::from(r) <= 262_144
                    && u64::from(n) * u64::from(r) * u64::from(p) <= 524_288
                    && (16..=64).contains(&dk.len())
                    && (8..=128).contains(&salt.len()),
                "Invalid or unsupported dashboard.basic_auth.password_hash"
            );
            (
                scrypt::Params::new(n.ilog2() as u8, r, p, dk.len())
                    .map_err(|_| invalid_password_hash())?,
                salt,
                dk,
            )
        } else {
            let params = scrypt::Params::new(14, 8, 1, 32).map_err(|_| invalid_password_hash())?;
            let mut salt = vec![0; 16];
            random()
                .fill(&mut salt)
                .map_err(|_| anyhow::anyhow!("System randomness unavailable"))?;
            let mut derived = vec![0; 32];
            let password = password.context("Missing dashboard password")?;
            scrypt::scrypt(password.as_bytes(), &salt, &params, &mut derived)
                .map_err(|_| invalid_password_hash())?;
            (params, salt, derived)
        };
        Ok(Some(Self {
            username,
            params,
            salt,
            derived,
            ttl: Duration::from_secs(ttl),
        }))
    }

    fn verify(&self, username: &str, password: &str) -> bool {
        let mut actual = vec![0; self.derived.len()];
        let derived = scrypt::scrypt(password.as_bytes(), &self.salt, &self.params, &mut actual);
        let username_ok = same_secret(username, &self.username);
        let password_ok =
            subtle::ConstantTimeEq::ct_eq(actual.as_slice(), self.derived.as_slice()).into();
        derived.is_ok() && username_ok && password_ok
    }
}

fn invalid_password_hash() -> anyhow::Error {
    anyhow::anyhow!("Invalid dashboard password hash parameters")
}

fn setting(key: &str, configured: Option<String>) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or(configured)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

struct Published {
    host: Authority,
    origin: String,
    prefix: String,
}

#[derive(Clone)]
struct LoginSession {
    identity: Identity,
    expires: Instant,
    revoked: CancellationToken,
}
struct NativeRequest {
    challenge: String,
    redirect: reqwest::Url,
    state: String,
    expires: Instant,
}
struct Code {
    challenge: String,
    expires: Instant,
}

#[derive(Default)]
struct SignIns {
    browser: HashMap<String, LoginSession>,
    native: HashMap<String, LoginSession>,
    refresh: HashMap<String, Instant>,
    pending: HashMap<String, NativeRequest>,
    codes: HashMap<String, Code>,
    attempts: VecDeque<Instant>,
}

impl SignIns {
    fn prune(&mut self) {
        let now = Instant::now();
        self.browser.retain(|_, session| session.expires > now);
        self.native.retain(|_, session| session.expires > now);
        self.refresh.retain(|_, expires| *expires > now);
        self.pending.retain(|_, request| request.expires > now);
        self.codes.retain(|_, code| code.expires > now);
    }
}

pub(super) struct Auth {
    published: Option<Published>,
    deployment_token: Option<String>,
    password: Option<Arc<Password>>,
    cookie: String,
    sign_ins: StdMutex<SignIns>,
}

impl Auth {
    pub(super) fn deployment_secret(&self) -> Option<&str> {
        self.deployment_token.as_deref()
    }

    pub(super) fn load(root: &Path, local_token: &str, address: SocketAddr) -> Result<Self> {
        let config: ConfigFile = match File::open(root.join("config.yaml")) {
            Ok(file) => serde_yaml::from_reader::<_, Option<ConfigFile>>(file)
                .map_err(|_| anyhow::anyhow!("Could not parse dashboard settings in config.yaml"))?
                .unwrap_or_default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(error) => return Err(error).context("Could not open profile config.yaml"),
        };
        let published = setting("HERMES_DASHBOARD_PUBLIC_URL", config.webapp.public_url)
            .map(|raw| -> Result<Published> {
                let parsed = reqwest::Url::parse(&raw)
                    .map_err(|_| anyhow::anyhow!("Invalid Webapp public URL"))?;
                ensure!(
                    parsed.scheme() == "https"
                        && parsed.host_str().is_some()
                        && parsed.username().is_empty()
                        && parsed.password().is_none()
                        && parsed.query().is_none()
                        && parsed.fragment().is_none(),
                    "The Webapp public URL must be an HTTPS origin with an optional mount path"
                );
                let origin = parsed.origin().ascii_serialization();
                let host: Authority = origin["https://".len()..].parse()?;
                ensure!(
                    !local_host(&host, address),
                    "The Webapp public authority must differ from the local listener"
                );
                let prefix = parsed.path().trim_end_matches('/').to_owned();
                ensure!(
                    prefix
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
                        && !prefix.contains("//"),
                    "Invalid Webapp mount path"
                );
                Ok(Published {
                    host,
                    origin,
                    prefix,
                })
            })
            .transpose()?;
        let deployment_token = if config.dashboard.session_token_auth.enabled == Some(false) {
            None
        } else {
            setting("HERMES_DASHBOARD_SESSION_TOKEN", None)
                .map(|token| -> Result<String> {
                    let token = match token.strip_prefix("file:") {
                        Some(path) => fs::read_to_string(path)
                            .context("Could not read dashboard session token file")?
                            .trim_start_matches('\u{feff}')
                            .trim()
                            .to_owned(),
                        None => token,
                    };
                    ensure!(
                        !token.is_empty()
                            && token.len() <= 4096
                            && token.bytes().all(|b| b.is_ascii_graphic()),
                        "Invalid dashboard session token"
                    );
                    ensure!(
                        !same_secret(&token, local_token),
                        "Dashboard and native owner tokens must differ"
                    );
                    Ok(token)
                })
                .transpose()?
        };
        let password = Password::load(config.dashboard.basic_auth)?.map(Arc::new);
        ensure!(
            published.is_none() || deployment_token.is_some() || password.is_some(),
            "Configure dashboard authentication before setting a Webapp public URL"
        );
        let hash = digest::digest(&digest::SHA256, root.as_os_str().as_encoded_bytes());
        let suffix: String = hash.as_ref()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let cookie = format!("__Secure-hermes_session_at_{suffix}");
        Ok(Self {
            published,
            deployment_token,
            password,
            cookie,
            sign_ins: StdMutex::new(SignIns::default()),
        })
    }

    pub(super) fn audience(
        &self,
        shared: &Shared,
        headers: &HeaderMap,
        peer: SocketAddr,
    ) -> std::result::Result<Audience, StatusCode> {
        if !peer.ip().is_loopback() {
            return Err(StatusCode::FORBIDDEN);
        }
        let host = one_header(headers, header::HOST)
            .filter(|s| !s.contains('@'))
            .and_then(|s| s.parse::<Authority>().ok())
            .ok_or(StatusCode::FORBIDDEN)?;
        let origin = if headers.contains_key(header::ORIGIN) {
            Some(one_header(headers, header::ORIGIN).ok_or(StatusCode::FORBIDDEN)?)
        } else {
            None
        };
        if let Some(published) = &self.published
            && host.host().eq_ignore_ascii_case(published.host.host())
            && host.port_u16().unwrap_or(443) == published.host.port_u16().unwrap_or(443)
        {
            if origin.is_none_or(|origin| origin == published.origin) {
                return Ok(Audience::Remote);
            }
        } else if local_host(&host, shared.address)
            && origin.is_none_or(|origin| local_origin(origin, shared.address))
        {
            // A proxy cannot use forwarded headers as proof of either audience.
            // Reject its claim to the local surface, even if its TCP peer is loopback.
            const FORWARDED: &[&str] = &[
                "forwarded",
                "x-forwarded-host",
                "x-forwarded-proto",
                "x-forwarded-for",
                "x-forwarded-port",
                "x-forwarded-prefix",
                "x-real-ip",
            ];
            if !FORWARDED.iter().any(|name| headers.contains_key(*name)) {
                return Ok(Audience::Local);
            }
        }
        Err(StatusCode::FORBIDDEN)
    }

    fn authorize(
        &self,
        shared: &Shared,
        headers: &HeaderMap,
        peer: SocketAddr,
    ) -> std::result::Result<Access, Response> {
        let audience = self
            .audience(shared, headers, peer)
            .map_err(IntoResponse::into_response)?;
        let credentials = credential_count(headers);
        if credentials > 1 {
            return Err(unauthorized());
        }
        if audience == Audience::Local {
            if header_token(headers).is_some_and(|token| same_secret(token, &shared.token)) {
                return Ok(Access {
                    audience,
                    revoked: None,
                    identity: Identity {
                        provider: "loopback",
                        user: "local".into(),
                        label: "Local".into(),
                        expires_at: 0,
                    },
                });
            }
            return Err(unauthorized());
        }
        if credentials == 1 {
            if let Some(token) = header_token(headers) {
                if self
                    .deployment_token
                    .as_deref()
                    .is_some_and(|secret| same_secret(token, secret))
                {
                    return Ok(Access {
                        audience,
                        revoked: None,
                        identity: Identity {
                            provider: "session-token",
                            user: "deployment-token".into(),
                            label: "Deployment session token".into(),
                            expires_at: unix_seconds() + 86_400,
                        },
                    });
                }
                if headers.contains_key(header::AUTHORIZATION)
                    && let Some(session) = lock(&self.sign_ins)
                        .native
                        .get(token)
                        .filter(|s| s.expires > Instant::now())
                        .cloned()
                {
                    return Ok(Access {
                        audience,
                        revoked: Some(session.revoked),
                        identity: session.identity,
                    });
                }
            }
        } else if let Some(token) = self.read_cookie(headers, &self.cookie)
            && let Some(session) = lock(&self.sign_ins)
                .browser
                .get(token)
                .filter(|s| s.expires > Instant::now())
                .cloned()
        {
            return Ok(Access {
                audience,
                revoked: Some(session.revoked),
                identity: session.identity,
            });
        }
        Err(unauthorized())
    }

    fn publication<'a>(
        &'a self,
        shared: &Shared,
        headers: &HeaderMap,
        peer: SocketAddr,
    ) -> std::result::Result<&'a Published, Response> {
        match self.audience(shared, headers, peer) {
            Ok(Audience::Remote) => Ok(self.published.as_ref().expect("published audience")),
            Ok(Audience::Local) => Err(error(
                StatusCode::NOT_FOUND,
                "No remote sign-in on this authority",
            )),
            Err(status) => Err(status.into_response()),
        }
    }

    fn read_cookie<'a>(&self, headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        let mut values = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .flat_map(|h| h.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .filter_map(|(key, value)| (key == name).then_some(value));
        let value = values.next()?;
        values.next().is_none().then_some(value)
    }
}

fn local_host(host: &Authority, address: SocketAddr) -> bool {
    matches!(host.host(), "localhost" | "127.0.0.1" | "[::1]" | "::1")
        && host.port_u16() == Some(address.port())
}
fn local_origin(origin: &str, address: SocketAddr) -> bool {
    matches!(origin, "file://" | "app://hermes")
        || matches!(
            origin.strip_prefix("http://").and_then(|s| s.parse::<Authority>().ok()), Some(host)
                if local_host(&host, address) && !host.as_str().contains('@')
        )
}
pub(super) fn credential_count(headers: &HeaderMap) -> usize {
    let authorization = headers.get_all(header::AUTHORIZATION);
    let count = authorization.iter().count();
    if count > 1 {
        return count;
    }
    // A proxy's HTTP Basic header is independent of the Hermes session header.
    let bearer = authorization
        .iter()
        .any(|value| value.to_str().is_ok_and(|s| s.starts_with("Bearer ")));
    usize::from(bearer) + headers.get_all("x-hermes-session-token").iter().count()
}

fn one_header(headers: &HeaderMap, name: axum::http::HeaderName) -> Option<&str> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}
fn random() -> SystemRandom {
    SystemRandom::new()
}
fn secret() -> String {
    let mut bytes = [0; 32];
    random()
        .fill(&mut bytes)
        .expect("System randomness unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}
fn unix_seconds() -> u64 {
    std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .unwrap_or_default()
        .as_secs()
}
pub(super) fn protected(value: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}
pub(super) fn error(status: StatusCode, detail: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"detail":detail})),
    )
        .into_response()
}
fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error":"unauthenticated","reason":"missing_or_invalid","detail":"Unauthorized"}))).into_response()
}

pub(super) async fn me(access: Access) -> Response {
    protected(access.identity.value())
}

pub(super) async fn status(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let auth = &shared.auth;
    let audience = match auth.audience(&shared, &headers, peer) {
        Ok(audience) => audience,
        Err(status) => return status.into_response(),
    };
    let mut providers = Vec::new();
    if auth.password.is_some() {
        providers.push("basic");
    }
    if auth.deployment_token.is_some() {
        providers.push("session-token");
    }
    let gated = audience == Audience::Remote;
    let flows = if gated && auth.password.is_some() {
        vec!["cookie", "native_pkce"]
    } else if gated {
        vec!["cookie"]
    } else {
        Vec::new()
    };
    let mut status = json!({"version":env!("CARGO_PKG_VERSION"),"implementation":"hermes-rust",
        "gateway_running":true,"gateway_state":"running","auth_required":gated,
        "auth_flows":flows,"auth_providers":providers,"peer_access":0});
    if auth.authorize(&shared, &headers, peer).is_ok() {
        let root = shared.library.root().to_owned();
        if let Ok(Ok(id)) = tokio::task::spawn_blocking(move || upload::install_id(&root)).await {
            status["install_id"] = json!(id);
        }
    }
    protected(status)
}

pub(super) async fn landing(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let auth = &shared.auth;
    let published = match auth.publication(&shared, &headers, peer) {
        Ok(published) => published,
        Err(response) => return response,
    };
    match auth.authorize(&shared, &headers, peer) {
        Ok(_) => (
            [(header::CACHE_CONTROL, "no-store")],
            Html("<!doctype html><title>Hermes</title><h1>Signed in to Hermes</h1>"),
        )
            .into_response(),
        Err(_) if auth.password.is_some() => {
            Redirect::to(&format!("{}/login", published.prefix)).into_response()
        }
        Err(response) => response,
    }
}

pub(super) async fn providers(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = shared.auth.audience(&shared, &headers, peer) {
        return status.into_response();
    }
    let providers = shared.auth.password.as_ref().map(
        |_| json!({"name":"basic","display_name":"Username / Password","supports_password":true}),
    );
    if providers.is_none() && shared.auth.deployment_token.is_none() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no auth providers registered",
        );
    }
    protected(json!({"providers":providers.into_iter().collect::<Vec<_>>()}))
}

fn cookie(response: &mut Response, published: &Published, key: &str, value: &str, age: u64) {
    let path = if published.prefix.is_empty() {
        "/"
    } else {
        &published.prefix
    };
    let value =
        format!("{key}={value}; Path={path}; Max-Age={age}; Secure; HttpOnly; SameSite=Lax");
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&value).expect("valid cookie"),
    );
}

fn login_form() -> Response {
    let nonce = secret();
    let html = format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8">
<title>Sign in to Hermes</title><h1>Sign in to Hermes</h1>
<form id="login"><label>Username <input name="username" autocomplete="username" required></label>
<label>Password <input name="password" type="password" autocomplete="current-password" required></label>
<button>Sign in</button><p role="alert" id="error"></p></form>
<script nonce="{nonce}">
const form = document.getElementById('login');
form.addEventListener('submit', async event => {{
  event.preventDefault();
  const data = Object.fromEntries(new FormData(form));
  data.provider = 'basic';
  data.next = new URLSearchParams(location.search).get('next') || '';
  const output = document.getElementById('error');
  try {{
    const res = await fetch('auth/password-login', {{method:'POST', headers:{{'Content-Type':'application/json'}},
      body:JSON.stringify(data)}});
    const result = await res.json();
    if (!res.ok) throw new Error(result.detail || 'Sign-in failed');
    location.assign(result.next);
  }} catch (error) {{ output.textContent = error.message; }}
}});
</script></html>"#
    );
    ([(header::CACHE_CONTROL, "no-store"),
        (header::CONTENT_SECURITY_POLICY, &format!("default-src 'none'; connect-src 'self'; script-src 'nonce-{nonce}'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"))],
        Html(html)).into_response()
}

pub(super) async fn login(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = shared.auth.publication(&shared, &headers, peer) {
        return response;
    }
    if shared.auth.password.is_none() {
        return error(StatusCode::NOT_FOUND, "No interactive sign-in provider");
    }
    login_form()
}

#[derive(Deserialize)]
pub(super) struct NativeAuthorize {
    provider: Option<String>,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    code_challenge_method: String,
}

fn pkce_text(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

fn loopback_redirect(raw: &str) -> Option<reqwest::Url> {
    let uri = reqwest::Url::parse(raw).ok()?;
    let loopback = uri.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    (raw.len() <= 2048
        && loopback
        && uri.scheme() == "http"
        && uri.port().is_some()
        && uri.username().is_empty()
        && uri.password().is_none()
        && uri.fragment().is_none()
        && !uri
            .query_pairs()
            .any(|(k, _)| matches!(&*k, "code" | "state")))
    .then_some(uri)
}

pub(super) async fn native_authorize(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<NativeAuthorize>,
) -> Response {
    let auth = &shared.auth;
    let published = match auth.publication(&shared, &headers, peer) {
        Ok(published) => published,
        Err(response) => return response,
    };
    if auth.password.is_none()
        || query
            .provider
            .as_deref()
            .is_some_and(|s| !s.is_empty() && s != "basic")
    {
        return error(StatusCode::NOT_FOUND, "Unknown provider");
    }
    let Some(redirect) = loopback_redirect(&query.redirect_uri) else {
        return error(StatusCode::BAD_REQUEST, "Invalid native redirect_uri");
    };
    if query.code_challenge_method != "S256"
        || !pkce_text(&query.code_challenge)
        || query.state.is_empty()
        || query.state.len() > 512
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid native authorization parameters",
        );
    }
    let handle = secret();
    let mut sign_ins = lock(&auth.sign_ins);
    sign_ins.prune();
    if sign_ins.pending.len() >= 64 {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pending native authorizations",
        );
    }
    sign_ins.pending.insert(
        handle.clone(),
        NativeRequest {
            challenge: query.code_challenge,
            redirect,
            state: query.state,
            expires: Instant::now() + Duration::from_secs(600),
        },
    );
    let mut response = Redirect::to(&format!("{}/login", published.prefix)).into_response();
    cookie(
        &mut response,
        published,
        &format!("{}_pkce", auth.cookie),
        &handle,
        600,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
pub(super) struct PasswordLogin {
    provider: String,
    username: String,
    password: String,
    #[serde(default)]
    next: String,
}

fn login_target(prefix: &str, next: &str) -> String {
    let safe = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains('\\')
        && !next.bytes().any(|b| b.is_ascii_control())
        && !["%2f", "%5c", "%0d", "%0a"]
            .iter()
            .any(|s| next.to_ascii_lowercase().contains(s));
    if !safe {
        return format!("{prefix}/");
    }
    if next == prefix
        || next
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with('/'))
    {
        return next.into();
    }
    format!("{prefix}{next}")
}

pub(super) async fn password_login(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<PasswordLogin>,
) -> Response {
    let auth = &shared.auth;
    let published = match auth.publication(&shared, &headers, peer) {
        Ok(published) => published,
        Err(response) => return response,
    };
    let Some(password) = auth.password.clone().filter(|_| body.provider == "basic") else {
        return error(StatusCode::NOT_FOUND, "Unknown provider");
    };
    if body.username.len() > 512 || body.password.len() > 4096 {
        return error(StatusCode::UNAUTHORIZED, "Invalid credentials");
    }
    {
        let mut sign_ins = lock(&auth.sign_ins);
        sign_ins
            .attempts
            .retain(|t| t.elapsed() < Duration::from_secs(60));
        // The actual peer is the local proxy. A forwarded address cannot bypass this limit.
        if sign_ins.attempts.len() >= 10 {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many login attempts. Try again shortly.",
            );
        }
        sign_ins.attempts.push_back(Instant::now());
    }
    let verified = {
        let password = password.clone();
        tokio::task::spawn_blocking(move || password.verify(&body.username, &body.password)).await
    };
    if !matches!(verified, Ok(true)) {
        return error(StatusCode::UNAUTHORIZED, "Invalid credentials");
    }
    let challenge_cookie = format!("{}_pkce", auth.cookie);
    let mut sign_ins = lock(&auth.sign_ins);
    sign_ins.prune();
    if let Some(handle) = auth.read_cookie(&headers, &challenge_cookie) {
        let Some(mut pending) = sign_ins.pending.remove(handle) else {
            return error(
                StatusCode::BAD_REQUEST,
                "Native login expired or unknown; restart sign-in.",
            );
        };
        if sign_ins.codes.len() >= 256 {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many pending native login codes",
            );
        }
        let code = secret();
        pending
            .redirect
            .query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &pending.state);
        sign_ins.codes.insert(
            code,
            Code {
                challenge: pending.challenge,
                expires: Instant::now() + Duration::from_secs(120),
            },
        );
        let mut response = protected(json!({"ok":true,"next":pending.redirect.as_str()}));
        cookie(&mut response, published, &challenge_cookie, "", 0);
        return response;
    }
    if sign_ins.browser.len() >= 256 {
        return error(StatusCode::TOO_MANY_REQUESTS, "Too many sign-ins");
    }
    let token = secret();
    if let Some(previous) = auth.read_cookie(&headers, &auth.cookie)
        && let Some(session) = sign_ins.browser.remove(previous)
    {
        session.revoked.cancel();
    }
    sign_ins
        .browser
        .insert(token.clone(), new_session(&password));
    let mut response =
        protected(json!({"ok":true,"next":login_target(&published.prefix, &body.next)}));
    cookie(
        &mut response,
        published,
        &auth.cookie,
        &token,
        password.ttl.as_secs(),
    );
    response
}

fn new_session(password: &Password) -> LoginSession {
    LoginSession {
        identity: Identity {
            provider: "basic",
            user: password.username.clone(),
            label: password.username.clone(),
            expires_at: unix_seconds() + password.ttl.as_secs(),
        },
        expires: Instant::now() + password.ttl,
        revoked: CancellationToken::new(),
    }
}

#[derive(Deserialize)]
pub(super) struct NativeToken {
    code: String,
    code_verifier: String,
}

fn native_tokens(sign_ins: &mut SignIns, password: &Password) -> Response {
    if sign_ins.native.len() >= 512 || sign_ins.refresh.len() >= 256 {
        return error(StatusCode::TOO_MANY_REQUESTS, "Too many sign-ins");
    }
    let access = secret();
    let refresh = secret();
    let session = new_session(password);
    let response = protected(
        json!({"access_token":access,"refresh_token":refresh,"token_type":"Bearer",
        "expires_at":session.identity.expires_at,"provider":"basic","user_id":session.identity.user}),
    );
    sign_ins.native.insert(access, session);
    sign_ins
        .refresh
        .insert(refresh, Instant::now() + Duration::from_secs(30 * 86_400));
    response
}

pub(super) async fn native_token(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<NativeToken>,
) -> Response {
    let auth = &shared.auth;
    if let Err(response) = auth.publication(&shared, &headers, peer) {
        return response;
    }
    let Some(password) = auth.password.as_ref() else {
        return error(StatusCode::NOT_FOUND, "Unknown provider");
    };
    let mut sign_ins = lock(&auth.sign_ins);
    sign_ins.prune();
    let Some(code) = sign_ins.codes.remove(&body.code) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Native login expired or unknown; restart sign-in.",
        );
    };
    let digest = digest::digest(&digest::SHA256, body.code_verifier.as_bytes());
    if !pkce_text(&body.code_verifier)
        || !same_secret(&code.challenge, &URL_SAFE_NO_PAD.encode(digest))
    {
        return error(StatusCode::BAD_REQUEST, "Invalid native login code");
    }
    native_tokens(&mut sign_ins, password)
}

#[derive(Deserialize)]
pub(super) struct Refresh {
    refresh_token: String,
}

pub(super) async fn native_refresh(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Refresh>,
) -> Response {
    let auth = &shared.auth;
    if let Err(response) = auth.publication(&shared, &headers, peer) {
        return response;
    }
    let Some(password) = auth.password.as_ref() else {
        return error(StatusCode::NOT_FOUND, "Unknown provider");
    };
    let mut sign_ins = lock(&auth.sign_ins);
    sign_ins.prune();
    if body.refresh_token.is_empty() {
        return error(StatusCode::BAD_REQUEST, "refresh_token required");
    }
    if sign_ins.refresh.remove(&body.refresh_token).is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(
                json!({"error":"session_expired","reason":"invalid_refresh_token",
                "detail":"Refresh token expired or invalid; start a new sign-in."}),
            ),
        )
            .into_response();
    }
    native_tokens(&mut sign_ins, password)
}

pub(super) async fn logout(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let auth = &shared.auth;
    let published = match auth.publication(&shared, &headers, peer) {
        Ok(published) => published,
        Err(response) => return response,
    };
    let mut sign_ins = lock(&auth.sign_ins);
    if let Some(token) = auth.read_cookie(&headers, &auth.cookie)
        && let Some(session) = sign_ins.browser.remove(token)
    {
        session.revoked.cancel();
    }
    let pkce = format!("{}_pkce", auth.cookie);
    if let Some(token) = auth.read_cookie(&headers, &pkce) {
        sign_ins.pending.remove(token);
    }
    let mut response = Redirect::to(&format!("{}/login", published.prefix)).into_response();
    cookie(&mut response, published, &auth.cookie, "", 0);
    cookie(&mut response, published, &pkce, "", 0);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) struct Ticket {
    issued: Instant,
    audience: Audience,
    local_native: bool,
    revoked: Option<CancellationToken>,
}

#[derive(Default)]
pub(super) struct Tickets(StdMutex<HashMap<String, Ticket>>);
impl Tickets {
    pub(super) fn issue_native(&self) -> Option<String> {
        self.issue(Audience::Local, true, None)
    }

    fn issue(
        &self,
        audience: Audience,
        local_native: bool,
        revoked: Option<CancellationToken>,
    ) -> Option<String> {
        let mut tickets = lock(&self.0);
        tickets.retain(|_, ticket| ticket.valid());
        if tickets.len() >= 256 {
            return None;
        }
        let id = secret();
        tickets.insert(
            id.clone(),
            Ticket {
                issued: Instant::now(),
                audience,
                local_native,
                revoked,
            },
        );
        Some(id)
    }

    pub(super) fn redeem(&self, token: &str, audience: Audience) -> Option<Ticket> {
        let mut tickets = lock(&self.0);
        if tickets.get(token)?.audience != audience {
            return None;
        }
        tickets.remove(token).filter(Ticket::valid)
    }

    pub(super) fn mint(&self, access: Access) -> Response {
        match self.issue(access.audience, false, access.revoked) {
            Some(ticket) => protected(json!({"ticket":ticket,"ttl_seconds":TICKET_TTL.as_secs()})),
            None => StatusCode::TOO_MANY_REQUESTS.into_response(),
        }
    }
}

impl Ticket {
    fn valid(&self) -> bool {
        self.issued.elapsed() < TICKET_TTL
            && self
                .revoked
                .as_ref()
                .is_none_or(|token| !token.is_cancelled())
    }

    pub(super) fn local_native(&self) -> bool {
        self.local_native
    }

    pub(super) fn revocation(&self) -> CancellationToken {
        self.revoked.clone().unwrap_or_default()
    }
}
