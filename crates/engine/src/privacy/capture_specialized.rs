//! Structured credential shapes that appear in captured shell text.
use super::{
    capture_assignment_value, capture_key_words, is_capture_marker, mask_capture_token,
    should_mask_capture_assignment,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use regex::{Captures, Regex};
use std::{borrow::Cow, sync::LazyLock};

const KEY_MASK: &str = "[REDACTED PRIVATE KEY]";

pub(super) fn private_keys(text: &str) -> Cow<'_, str> {
    if ![
        "PRIVATE KEY",
        "LS0tLS1CRUdJTi",
        "Private-Lines:",
        "AGE-SECRET-KEY-1",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return Cow::Borrowed(text);
    }
    static BLOCK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?s)-----BEGIN[A-Z ]*PRIVATE KEY-----.*?-----END[A-Z ]*PRIVATE KEY-----|",
            r"-----BEGIN PGP PRIVATE KEY BLOCK-----.*?-----END PGP PRIVATE KEY BLOCK-----|",
            r"AGE-SECRET-KEY-1[0-9A-Z]{20,}"
        ))
        .unwrap()
    });
    static BASE64_PEM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"LS0tLS1CRUdJTi[A-Za-z0-9+/=]{60,}").unwrap());
    static PUTTY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"\b(Private-Lines:[ \t]*\d+[ \t]*(\r?\n|\\r\\n|\\n))",
            r"(?:[ \t]*(?:\d+(?:[|:]|\t)[ \t]*)?[A-Za-z0-9+/=]{16,}[ \t]*(?:\r?\n|\\r\\n|\\n)?)+"
        ))
        .unwrap()
    });
    let block = BLOCK.replace_all(text, KEY_MASK);
    let partial = if block.contains("PRIVATE KEY-----") {
        Cow::Owned(partial_pem(&block))
    } else {
        block
    };
    let pem = BASE64_PEM.replace_all(&partial, |groups: &Captures<'_>| {
        let input = &groups[0];
        let mut padded = input.to_owned();
        padded.extend(std::iter::repeat_n('=', (4 - input.len() % 4) % 4));
        if STANDARD.decode(padded).is_ok_and(|bytes| {
            bytes.starts_with(b"-----BEGIN")
                && bytes[..bytes.len().min(64)]
                    .windows(b"PRIVATE KEY".len())
                    .any(|part| part == b"PRIVATE KEY")
        }) {
            KEY_MASK.to_owned()
        } else {
            input.to_owned()
        }
    });
    let putty = PUTTY.replace_all(&pem, |groups: &Captures<'_>| {
        format!("{}{KEY_MASK}{}", &groups[1], &groups[2])
    });
    Cow::Owned(putty.into_owned())
}

fn partial_pem(text: &str) -> String {
    static ARMOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"-----(BEGIN|END)[A-Z ]*PRIVATE KEY-----").unwrap());
    static BODY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9+/=]{16,}$").unwrap());
    static TRAILER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:[A-Za-z0-9+/]{1,15}={0,2}|==?)$").unwrap());
    let mut lines: Vec<_> = text.split('\n').map(Some).collect();
    let is_body = |line: &str| {
        let line = line.trim();
        BODY.is_match(line) && !line.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    for row in 0..lines.len() {
        let Some(armor) = lines[row].and_then(|line| ARMOR.captures(line)) else {
            continue;
        };
        let forward = &armor[1] == "BEGIN";
        let step = if forward { 1 } else { -1 };
        let first = row as isize + step;
        let mut next = first;
        if !forward
            && next > 0
            && lines[next as usize].is_some_and(|line| TRAILER.is_match(line.trim()))
            && lines[next as usize - 1].is_some_and(is_body)
        {
            lines[next as usize] = None;
            next -= 1;
        }
        while let Some(Some(line)) = lines.get(next as usize) {
            if !is_body(line) {
                break;
            }
            lines[next as usize] = None;
            next += step;
        }
        if next != first {
            if forward
                && lines.get(next as usize).is_some_and(|line| {
                    line.is_some_and(|line| {
                        line.trim_end().ends_with('=') && TRAILER.is_match(line.trim())
                    })
                })
            {
                lines[next as usize] = None;
            }
            lines[row] = Some(KEY_MASK);
        }
    }
    lines.into_iter().flatten().collect::<Vec<_>>().join("\n")
}

pub(super) fn discord_tokens(text: &str) -> Cow<'_, str> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    static DISCORD: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(concat!(
            r"(?:mfa\.[A-Za-z0-9_-]{84}|[MNO][A-Za-z0-9_-]{22,27}",
            r"\.[A-Za-z0-9_-]{6,7}\.[A-Za-z0-9_-]{27,})(?![A-Za-z0-9_-]|\.[A-Za-z0-9_-])"
        ))
        .unwrap()
    });
    static ESCAPE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:\\[nrtbf]|\\x[0-9A-Fa-f]{2}|%[0-9A-Fa-f]{2}|\\u[0-9A-Fa-f]{4}|(?:\x1b|\\x1b|\\033|\\e)\[[0-9;]*m)$").unwrap()
    });
    DISCORD.replace_all(text, |groups: &fancy_regex::Captures<'_>| {
        let candidate = groups.get(0).unwrap();
        let token = candidate.as_str();
        let valid = if token.starts_with("mfa.") {
            let before = &text[..candidate.start()];
            !before
                .as_bytes()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric() || b"_.-".contains(c))
                || ESCAPE.is_match(before)
        } else {
            let (snowflake, _) = token.split_once('.').unwrap();
            URL_SAFE_NO_PAD.decode(snowflake).is_ok_and(|bytes| {
                (17..=20).contains(&bytes.len()) && bytes.iter().all(u8::is_ascii_digit)
            })
        };
        if valid {
            mask_capture_token(token)
        } else {
            token.to_owned()
        }
    })
}

pub(super) fn webhooks(text: &str) -> Cow<'_, str> {
    static HOOK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)(https?://hooks\.slack(?:-gov)?\.com\.?(?::\d+)?/workflows/[A-Z0-9]+/[A-Z0-9]+/\d+/)([A-Z0-9]{8,})|",
            r"(https?://hooks\.slack(?:-gov)?\.com\.?(?::\d+)?/(?:services|workflows|triggers)/[A-Z0-9]+/[A-Z0-9]+/)([A-Z0-9]{8,})|",
            r"(https?://(?:canary\.|ptb\.|www\.)?discord(?:app)?\.com\.?(?::\d+)?/api/(?:v\d+/)?webhooks/\d+/)([\w-]{8,})|",
            r"(https?://[A-Z0-9.-]*\.?(?:webhook\.office\.com\.?(?::\d+)?/webhookb2|outlook\.office\.com\.?(?::\d+)?/webhook)/)([\w@/%+=-]{8,})"
        ))
        .unwrap()
    });
    HOOK.replace_all(text, |groups: &Captures<'_>| {
        let mut parts = groups.iter().skip(1).flatten();
        let path = parts.next().unwrap();
        let token = parts.next().unwrap();
        format!("{}{}", path.as_str(), mask_capture_token(token.as_str()))
    })
}

pub(super) fn cookie_assignments(text: &str) -> Cow<'_, str> {
    static COOKIE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(concat!(
            r#"(?i)(?<![\w?&])((?:[A-Za-z0-9]+_)*cookie[ \t]*=(?!=)[ \t]*)"#,
            r#"(?:"((?:\\.|[^"\\\r\n])*)"|'((?:\\.|[^'\\\r\n])*)'|([^\s"';&|<>(),]+))"#
        ))
        .unwrap()
    });
    COOKIE.replace_all(text, |groups: &fancy_regex::Captures<'_>| {
        let (quote, value) = if let Some(value) = groups.get(2) {
            ("\"", value.as_str())
        } else if let Some(value) = groups.get(3) {
            ("'", value.as_str())
        } else {
            ("", &groups[4])
        };
        if cookie_credential(value, groups[1].contains('_')) {
            format!("{}{quote}{}{quote}", &groups[1], mask_capture_token(value))
        } else {
            groups[0].to_owned()
        }
    })
}

fn cookie_credential(value: &str, qualified: bool) -> bool {
    (value.contains('=') && !value.starts_with('$'))
        || should_mask_capture_assignment(if qualified { "session_token" } else { "token" }, value)
}

const HEADER_NAME: &str =
    r"(?:x-api-key|x-goog-api-key|api-key|apikey|x-api-token|x-auth-token|x-access-token)";
const COOKIE_NAME: &str = r"(?:set-)?cookie";
const COOKIE_PAIR: &str = r#"[!#$%&'*+\-.^_|\x60~0-9A-Za-z]+=(?:"[^"\r\n]*"|[^\s;\\'"`]+)*"#;

pub(super) fn request_headers(text: &str) -> String {
    static RAW: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"(?im)^([ \t]*(?:[<>][ \t]+)?(?:{HEADER_NAME}[ \t]*:|{COOKIE_NAME}:)[ \t]*)([^\r\n]+)"
        ))
        .unwrap()
    });
    static CURL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r#"(?im)(^|[ \t])((?:-H|--header)[ \t=]*)(?:(?:"({HEADER_NAME}|{COOKIE_NAME})(:[ \t]*)((?:\\.|[^"\\\r\n])+)"|'({HEADER_NAME}|{COOKIE_NAME})(:[ \t]*)([^'\r\n]+)'|({HEADER_NAME}|{COOKIE_NAME})(:)([^\s;&|<>()]+)))"#
        )).unwrap()
    });
    static INLINE_COOKIE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"(?i)(\b{COOKIE_NAME}:[ \t]*)({COOKIE_PAIR}(?:;[ \t]*{COOKIE_PAIR})*)"
        ))
        .unwrap()
    });
    static BARE_SET_COOKIE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(\bset-cookie:[ \t]*)[^\s\\"';&|<>()$\x60*=]{16,}([\s;]|$)"#).unwrap()
    });
    static INLINE_SECRET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r#"(?i)(\b{HEADER_NAME}[ \t]*:[ \t]*)([^\s"';&|<>()]+)"#
        ))
        .unwrap()
    });
    static PAIR_AT_START: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[!#$%&'*+\-.^_|\x60~0-9A-Za-z]+=").unwrap());
    let raw = RAW.replace_all(text, |groups: &Captures<'_>| {
        let header = &groups[1];
        let value = &groups[2];
        if value
            .split_whitespace()
            .next()
            .is_some_and(is_capture_marker)
            || (header.trim_end().to_ascii_lowercase().ends_with("cookie:")
                && !PAIR_AT_START.is_match(value))
        {
            groups[0].to_owned()
        } else {
            format!("{header}***")
        }
    });
    let curl = CURL.replace_all(&raw, |groups: &Captures<'_>| {
        let (base, quote) = if groups.get(3).is_some() {
            (3, "\"")
        } else if groups.get(6).is_some() {
            (6, "'")
        } else {
            (9, "")
        };
        let header = &groups[base];
        if header.to_ascii_lowercase().ends_with("cookie")
            && !PAIR_AT_START.is_match(&groups[base + 2])
        {
            return groups[0].to_owned();
        }
        format!(
            "{}{}{quote}{header}{}***{quote}",
            &groups[1],
            &groups[2],
            &groups[base + 1]
        )
    });
    let cookies =
        INLINE_COOKIE.replace_all(&curl, |groups: &Captures<'_>| format!("{}***", &groups[1]));

    let cookies = BARE_SET_COOKIE.replace_all(&cookies, |groups: &Captures<'_>| {
        format!("{}***{}", &groups[1], &groups[2])
    });
    INLINE_SECRET
        .replace_all(&cookies, |groups: &Captures<'_>| {
            format!("{}***", &groups[1])
        })
        .into_owned()
}

pub(super) fn structured_fields(text: &str) -> String {
    static JSON: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"("([A-Za-z0-9_.-]{1,128})"\s*:\s*")((?:\\.|[^"\\\r\n])+?)""#).unwrap()
    });
    static TRUNCATED_HEADER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r#"(?im)("(?:{HEADER_NAME}|{COOKIE_NAME})"\s*:\s*")((?:\\.|[^"\\\r\n])*(?:\\)?)$"#
        ))
        .unwrap()
    });
    static ESCAPED_JSON: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r#"((\\+)"([A-Za-z0-9_.-]{1,128})\2")\s*:\s*\2"([^"\\\r\n]+)\2""#)
            .unwrap()
    });
    static REPR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"'([A-Za-z_][A-Za-z0-9_]*)'(\s*:\s*)([bB]?)(?:"((?:\\.|[^"\\\r\n])+)"|'((?:\\.|[^'\\\r\n])+)'|())"#)
            .unwrap()
    });
    static API_HEADER_JSON: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(&format!(r"(?i)^{HEADER_NAME}$")).unwrap());

    let json = JSON.replace_all(text, |groups: &Captures<'_>| {
        let key = &groups[2];
        let encoded = &groups[3];
        let decoded = serde_json::from_str::<String>(&format!("\"{encoded}\"")).ok();
        let value = decoded.as_deref().unwrap_or(encoded);
        if is_capture_marker(value) {
            return groups[0].to_owned();
        }
        let name = key.to_ascii_lowercase();
        if matches!(name.as_str(), "cookie" | "set-cookie") || API_HEADER_JSON.is_match(key) {
            let valid = if name.ends_with("cookie") {
                cookie_credential(value, false)
            } else {
                !value.starts_with('$')
                    && !value.starts_with("os.getenv")
                    && !value.starts_with("process.env")
            };
            if valid {
                return format!("{}{}\"", &groups[1], json_mask(value));
            }
        } else if name == "apikey" && !value.starts_with('$') {
            return format!("{}***\"", &groups[1]);
        } else if is_structured_credential(key, value) {
            return format!("\"{key}\": \"{}\"", json_mask(value));
        }
        groups[0].to_owned()
    });
    let header = TRUNCATED_HEADER.replace_all(&json, |groups: &Captures<'_>| {
        format!("{}{}", &groups[1], json_mask(&groups[2]))
    });
    let escaped = ESCAPED_JSON.replace_all(&header, |groups: &fancy_regex::Captures<'_>| {
        if is_structured_credential(&groups[3], &groups[4]) {
            let slashes = &groups[2];
            format!(
                "{}: {slashes}\"{}{slashes}\"",
                &groups[1],
                json_mask(&groups[4])
            )
        } else {
            groups[0].to_owned()
        }
    });
    REPR.replace_all(&escaped, |groups: &Captures<'_>| {
        let (quote, value) = capture_assignment_value(groups, 4);
        if value.is_empty()
            || value.contains("***")
            || is_capture_marker(value)
            || !credential_key(&groups[1])
        {
            groups[0].to_owned()
        } else {
            format!(
                "'{}'{}{}{quote}***{quote}",
                &groups[1], &groups[2], &groups[3]
            )
        }
    })
    .into_owned()
}

fn json_mask(value: &str) -> String {
    let masked =
        serde_json::to_string(&mask_capture_token(value)).expect("a string serializes to JSON");
    masked[1..masked.len() - 1].to_owned()
}

fn is_structured_credential(key: &str, value: &str) -> bool {
    if is_capture_marker(value) || key.eq_ignore_ascii_case("auth") {
        return false;
    }
    let key = capture_key_words(key);
    let key = key.strip_suffix("_value").unwrap_or(&key);
    credential_key(key)
        && (should_mask_capture_assignment(key, value)
            || (qualified_key(key) && !value.starts_with(['$', '<']) && !is_config_literal(value)))
}

fn credential_key(key: &str) -> bool {
    let normalized = capture_key_words(key);
    matches!(
        normalized.as_str(),
        "authorization" | "bearer" | "key_material"
    ) || [
        "apikey",
        "api_key",
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "credentials",
    ]
    .iter()
    .any(|suffix| normalized.ends_with(suffix))
        || qualified_key(&normalized)
}

fn qualified_key(key: &str) -> bool {
    key.ends_with("_key")
        && key.split('_').any(|word| {
            matches!(
                word,
                "secret"
                    | "private"
                    | "signing"
                    | "encryption"
                    | "encrypt"
                    | "master"
                    | "hmac"
                    | "symmetric"
                    | "access"
            )
        })
}

fn is_config_literal(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "none" | "yes" | "no" | "default"
    )
}

pub(super) fn command_flags(text: &str) -> Cow<'_, str> {
    static FLAG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r#"(?i)(--(?:api[-_]?key|access[-_]?token|auth[-_]?token|client[-_]?secret|password|passwd|passphrase|secret|token)(?:=|[ \t]+))"#,
            r#"(?:"((?:\\.|[^"\\\r\n])+)"|'((?:\\.|[^'\\\r\n])+)'|([^\s"';&|<>]+))"#
        )).unwrap()
    });
    FLAG.replace_all(text, |groups: &Captures<'_>| {
        let (_, value) = capture_assignment_value(groups, 2);
        if value.starts_with(['$', '<', '{', '%', '/', '~'])
            || is_capture_marker(value)
            || value.chars().all(|c| c.is_ascii_digit())
            || is_config_literal(value)
        {
            groups[0].to_owned()
        } else {
            format!("{}{}", &groups[1], mask_capture_token(value))
        }
    })
}

pub(super) fn form_body(text: &str) -> Cow<'_, str> {
    static FORM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[A-Za-z_][A-Za-z0-9_.-]*=[^&\s]*(?:&[A-Za-z_][A-Za-z0-9_.-]*=[^&\s]*)+$")
            .unwrap()
    });
    let trimmed = text.trim();
    if text.contains('\n') || !FORM.is_match(trimmed) {
        return Cow::Borrowed(text);
    }
    let mut redacted = Vec::new();
    for entry in trimmed.split('&') {
        let (key, _) = entry.split_once('=').unwrap();
        if super::secret_parameter(key) {
            redacted.push(format!("{key}=***"));
        } else {
            redacted.push(entry.to_owned());
        }
    }
    Cow::Owned(redacted.join("&"))
}

pub(super) fn zhipu_keys(text: &str) -> Cow<'_, str> {
    static KEY: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(
            r"(?<![A-Za-z0-9_.-])[0-9a-f]{32}\.[A-Za-z0-9]{16,}(?![A-Za-z0-9_.-])",
        )
        .unwrap()
    });
    if text.contains('.') {
        KEY.replace_all(text, |groups: &fancy_regex::Captures<'_>| {
            mask_capture_token(&groups[0])
        })
    } else {
        Cow::Borrowed(text)
    }
}

pub(super) fn signin_links(text: &str) -> Cow<'_, str> {
    static LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"((?:^|[^A-Za-z0-9_-])hermes-session=|/webapp/launch#(?:[^\s#]*?&)?ticket=)[A-Za-z0-9_-]{20,}",
        )
        .unwrap()
    });
    LINK.replace_all(text, |groups: &Captures<'_>| format!("{}***", &groups[1]))
}

pub(super) fn azure_credentials(text: &str) -> String {
    // Spaced assignments are ambiguous. Stock only treats them as storage keys when the
    // value looks like a base64 credential and contains a digit.
    const ASSIGN: &str =
        r"(?:=|[ \t]*=[ \t]*(?=[A-Za-z+/]*[0-9])(?=[A-Za-z0-9+/]{16,}={0,2}(?![\w.(])))";
    static KEYS: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(&format!(
            r#"(?i)((?<![^;,?&'"\s])(?:AccountKey|SharedAccessKey){ASSIGN})([^;\s'\"&]+)"#
        ))
        .unwrap()
    });
    static SIGNATURE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(&format!(
            r#"(?i)((?<![^;,?&'"\s])SharedAccessSignature{ASSIGN})((?:(?!sig=)[^;\s'"&),\x60|]+&)*(?:sig=)?[^;\s'"&),\x60|]*)"#
        ))
        .unwrap()
    });
    let mask = |groups: &fancy_regex::Captures<'_>| {
        let value = groups[2].trim_matches(['\'', '"']);
        if value.is_empty()
            || is_capture_marker(value)
            || value.starts_with(['$', '<', '{', '%'])
            || value.bytes().all(|byte| byte.is_ascii_digit())
        {
            groups[0].to_owned()
        } else {
            format!("{}{}", &groups[1], mask_capture_token(value))
        }
    };
    let keys = KEYS.replace_all(text, mask);
    SIGNATURE.replace_all(&keys, mask).into_owned()
}

pub(super) fn connection_passwords(text: &str) -> String {
    const COMPANIONS: &str = concat!(
        r"host|server|data\s+source|address|addr|network\s+address|user\s*id|uid|",
        r"user(?:name)?|database|dbname|hostaddr|initial\s+catalog|port"
    );
    static PASSWORD: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r#"(?i)(?<![^\s;"'])(?:password|pwd)[ \t]*=[ \t]*"#).unwrap()
    });
    static COMPANION: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(&format!(r#"(?i)(?<![^\s;"'])(?:{COMPANIONS})[ \t]*="#)).unwrap()
    });
    static SEMICOLON: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(concat!(
            r"(?i);[ \t]*(?:{0}|password|pwd|driver|encrypt|pooling|",
            r"integrated\s+security|trusted_connection|trustservercertificate|sslmode|ssl\s+mode|",
            r"timeout|connect(?:ion)?\s+timeout|application\s+name|search\s+path)[ \t]*="
        ), COMPANIONS)).unwrap()
    });

    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if !PASSWORD.is_match(line).unwrap_or(false) || !COMPANION.is_match(line).unwrap_or(false) {
            output.push_str(line);
            continue;
        }
        let semicolon = SEMICOLON.is_match(line);
        let mut written = 0;
        for field in PASSWORD.find_iter(line).flatten() {
            if field.start() < written {
                continue;
            }
            let start = field.end();
            let end = connection_value_end(line.as_bytes(), start, semicolon);
            let (key, padding) = field.as_str().split_once('=').unwrap();
            if start == end || (!semicolon && !padding.is_empty() && !key.ends_with([' ', '\t'])) {
                continue;
            }
            output.push_str(&line[written..start]);
            let value = &line.as_bytes()[start..end];
            match value[0] {
                b'{' if semicolon => output.push_str("{***}"),
                quote @ (b'\'' | b'"') if value.len() > 1 && value.last() == Some(&quote) => {
                    output.push(quote.into());
                    output.push_str("***");
                    output.push(quote.into());
                }
                _ => output.push_str("***"),
            }
            written = end;
        }
        output.push_str(&line[written..]);
    }
    output
}

fn connection_value_end(line: &[u8], start: usize, semicolon: bool) -> usize {
    let Some(&first) = line.get(start) else {
        return start;
    };
    if matches!(first, b'\'' | b'"') || (semicolon && first == b'{') {
        let close = if first == b'{' { b'}' } else { first };
        let mut index = start + 1;
        while let Some(&byte) = line.get(index) {
            if !semicolon && byte == b'\\' {
                index += 2;
            } else if byte == close {
                if !semicolon || line.get(index + 1) != Some(&close) {
                    return index + 1;
                }
                index += 2;
            } else {
                index += 1;
            }
        }
        return line.len();
    }

    let mut end = start;
    while let Some(&byte) = line.get(end) {
        if !semicolon && byte == b'\\' {
            end += 2;
            continue;
        }
        let delimiter: &[u8] = if semicolon { b";\r\n" } else { b" \r\n\t" };
        let ends_wrapper = matches!(byte, b'\'' | b'"')
            && line
                .get(end + 1)
                .is_none_or(|next| b" \t\r\n,;)]}>/.+&|".contains(next));
        if delimiter.contains(&byte) || ends_wrapper {
            break;
        }
        end += 1;
    }
    end = end.min(line.len());
    if semicolon {
        while end > start && matches!(line[end - 1], b' ' | b'\t') {
            end -= 1;
        }
    }
    end
}
