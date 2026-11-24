//! Model-side text filtering. Credentials used to authenticate the HTTPS
//! request and the user's unmodified local transcript stay outside this filter.
mod capture_specialized;

use regex::Regex;
use serde_json::Value;
use std::{borrow::Cow,sync::LazyLock};

const HIDDEN:&str="[REDACTED]";
static BLOCK:LazyLock<Regex>=LazyLock::new(||Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----").unwrap());
static TOKEN:LazyLock<Regex>=LazyLock::new(||Regex::new(r"\b(?:sk-(?:ant-|proj-|or-v1-)?[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{24,}|github_pat_[A-Za-z0-9_]{26,}|AKIA[A-Z0-9]{16}|xox[baprs]-[A-Za-z0-9-]{12,}|AIza[A-Za-z0-9_-]{30,})\b").unwrap());
static BEARER:LazyLock<Regex>=LazyLock::new(||Regex::new(r"(?i)(\b(?:Bearer|Basic)[ \t]+)[A-Za-z0-9._~+/=-]{8,}").unwrap());
static ASSIGN:LazyLock<Regex>=LazyLock::new(||Regex::new(r#"(?im)(\b[A-Za-z0-9_.-]*(?:api[_-]?key|access[_-]?token|secret|password|passwd|refresh[_-]?token|auth[_-]?token|private[_-]?key)["']?\s*[:=]\s*)(?:"[^"\r\n]+"|'[^'\r\n]+'|[^,;\s}\]]+)"#).unwrap());

pub(crate) fn contains_secret(text:&str)->bool{
    BLOCK.is_match(text)||TOKEN.is_match(text)||BEARER.is_match(text)||ASSIGN.is_match(text)
}
pub(crate) fn redact(text:&mut String){
    let block=BLOCK.replace_all(text,HIDDEN);
    let token=TOKEN.replace_all(&block,HIDDEN);
    let bearer=BEARER.replace_all(&token,"${1}[REDACTED]");
    let result=ASSIGN.replace_all(&bearer,"${1}[REDACTED]");
    if matches!(result,Cow::Owned(_))||matches!(bearer,Cow::Owned(_))||matches!(token,Cow::Owned(_))||matches!(block,Cow::Owned(_)){*text=result.to_string();}
}
/// Redact the complete captured stream before the caller takes its tail.
pub(crate) fn redact_capture(text: &str, protected: &[&str]) -> String {
    static AUTHORIZATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)((?:proxy-)?authorization:\s*)([A-Za-z][\w.+-]*[ \t]+)?([^\s"']+)"#)
            .unwrap()
    });
    static JWT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"eyJ[A-Za-z0-9_-]{10,}(?:\.[A-Za-z0-9_=-]{4,}){0,2}").unwrap()
    });
    static TELEGRAM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b(?:bot)?\d{8,}:[A-Za-z0-9_-]{30,}").unwrap());
    static PHONE: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(r"(?<![0-9A-Za-z_.])\+[1-9]\d{6,14}(?![A-Za-z0-9])").unwrap()
    });

    let mut text = text.to_owned();
    let mut protected: Vec<_> = protected
        .iter()
        .copied()
        .filter(|value| !value.is_empty())
        .collect();
    protected.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
    for value in protected {
        let mask = if CAPTURE_PROVIDER_TOKEN
            .find(value)
            .is_some_and(|token| token.start() == 0 && token.end() == value.len())
        {
            mask_capture_token(value)
        } else {
            HIDDEN.to_owned()
        };
        text = text.replace(value, &mask);
    }

    let keys = capture_specialized::private_keys(&text);
    let tokens = redact_capture_tokens(&keys);
    let zhipu = capture_specialized::zhipu_keys(&tokens);
    let discord = capture_specialized::discord_tokens(&zhipu);
    let assignments = redact_capture_assignments(&discord);
    let flags = capture_specialized::command_flags(&assignments);
    let azure = capture_specialized::azure_credentials(&flags);
    let auth = AUTHORIZATION.replace_all(&azure, |groups: &regex::Captures<'_>| {
        let token = &groups[3];
        if is_capture_marker(token)
            || matches!(
                token.to_ascii_lowercase().as_str(),
                "bearer" | "basic" | "bot" | "token" | "digest" | "negotiate" | "ntlm" | "oauth"
            )
        {
            return groups[0].to_owned();
        }
        let scheme = groups.get(2).map_or("", |value| value.as_str());
        format!("{}{scheme}{}", &groups[1], mask_capture_token(token))
    });
    let headers = capture_specialized::request_headers(&auth);
    let telegram = TELEGRAM.replace_all(&headers, |groups: &regex::Captures<'_>| {
        let (id, _) = groups[0]
            .split_once(':')
            .expect("Telegram credential contains a colon");
        format!("{id}:***")
    });
    let webhooks = capture_specialized::webhooks(&telegram);
    let passwords = capture_specialized::connection_passwords(&webhooks);
    let jwt = JWT.replace_all(&passwords, |groups: &regex::Captures<'_>| {
        mask_capture_token(&groups[0])
    });
    let signins = capture_specialized::signin_links(&jwt);
    let urls = redact_capture_urls(&signins);
    let form = capture_specialized::form_body(&urls);
    PHONE
        .replace_all(&form, |groups: &fancy_regex::Captures<'_>| {
            let phone: Vec<_> = groups[0].chars().collect();
            let keep = if phone.len() <= 8 { 2 } else { 4 };
            phone[..keep]
                .iter()
                .copied()
                .chain("****".chars())
                .chain(phone[phone.len() - keep..].iter().copied())
                .collect::<String>()
        })
        .into_owned()
}

fn mask_capture_token(token: &str) -> String {
    let token: Vec<_> = token
        .chars()
        .filter(|c| !matches!(c, '\x00'..='\x1f' | '\x7f'..='\u{9f}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'))
        .collect();
    if token.len() < 18 {
        "***".into()
    } else {
        token[..6]
            .iter()
            .copied()
            .chain("...".chars())
            .chain(token[token.len() - 4..].iter().copied())
            .collect()
    }
}

static CAPTURE_PROVIDER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:sk-[A-Za-z0-9_-](?:\.?[A-Za-z0-9_-]){9,}|ctx7sk-[A-Za-z0-9_-]{10,}|",
        r"gh[pousr]_[A-Za-z0-9]{10,}|github_pat_[A-Za-z0-9_]{10,}|",
        r"xapp-\d+-[A-Za-z0-9-]{10,}|xox[baprs]-[A-Za-z0-9-]{10,}|",
        r"AIza[A-Za-z0-9_-]{30,}|GOCSPX-[A-Za-z0-9_-]{8,}|ya29\.(?:[a-z]\.)?[A-Za-z0-9_-]{20,}|",
        r"AQ\.[A-Za-z0-9_-]{40,}|1//0[A-Za-z0-9_-]{20,}|",
        r"(?:pplx-|fc-|hf_|r8_|npm_|gsk_|syt_|retaindb_|hsk-|mem0_|brv_|ntn_|",
        r"tvly-(?:(?:dev|prod)-)?|exa_|dop_v1_|doo_v1_|rk_live_)[A-Za-z0-9]{10,}|",
        r"(?:pypi-|fal_|bb_live_|GR1348941)[A-Za-z0-9_-]{10,}|sk_[A-Za-z0-9_]{10,}|",
        r"SG\.[A-Za-z0-9_-]{10,}(?:\.[A-Za-z0-9_-]{10,})?|pk-lf-[A-Za-z0-9-]{8,}|",
        r"(?:xai-|fw[-_]|fpk_)[A-Za-z0-9]{30,}|am_(?:org_)?[A-Za-z0-9]{20,}|",
        r"gl(?:pat|oas|dt|cbt|ptt|ft|imt|agent|soat|ffct|wt)-[A-Za-z0-9_-]{10,}|",
        r"glrtr?-[A-Za-z0-9_.-]{10,}|glsa_[A-Za-z0-9]{10,}(?:_[A-Fa-f0-9]{8})?|",
        r"glc_eyJ[A-Za-z0-9+/]{29,}={0,3}|gAAAA[A-Za-z0-9_=-]{20,}|AKIA[A-Z0-9]{16})"
    ))
    .unwrap()
});

fn redact_capture_tokens(text: &str) -> String {
    static NOISE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?:\x1b\[|\u{9b})[0-?]*[ -/]*[@-~]|\x1b(?:[ -/]+[0-~]|[NO][^\x00-\x1f\x7f]|[78=>M])|",
            r"[\x00-\x1f\x7f\u{200b}-\u{200f}\u{2028}-\u{202f}\u{2060}\u{feff}]"
        ))
        .unwrap()
    });

    // Map matches in a control-free copy back to the original stream. Formatting outside
    // a credential's span, including its surrounding colour escapes, stays in the output.
    let mut shadow = String::new();
    let mut spans = Vec::new();
    let mut ends = Vec::new();
    let mut end = 0;
    for noise in NOISE.find_iter(text) {
        spans.push((shadow.len(), end));
        shadow.push_str(&text[end..noise.start()]);
        ends.push((shadow.len(), noise.start()));
        end = noise.end();
    }
    if spans.is_empty() {
        return CAPTURE_PROVIDER_TOKEN
            .replace_all(text, |groups: &regex::Captures<'_>| {
                mask_capture_token(&groups[0])
            })
            .into_owned();
    }
    spans.push((shadow.len(), end));
    shadow.push_str(&text[end..]);
    ends.push((shadow.len(), text.len()));

    let mut output = String::new();
    let mut consumed = 0;
    for token in CAPTURE_PROVIDER_TOKEN.find_iter(&shadow) {
        let boundary = spans.partition_point(|&(offset, _)| offset <= token.start()) - 1;
        let (shadow_start, source_start) = spans[boundary];
        let source_start = source_start + token.start() - shadow_start;
        let (shadow_end, source_end) =
            ends[ends.partition_point(|&(offset, _)| offset < token.end())];
        let source_end = source_end - (shadow_end - token.end());
        if let Some(line_end) = text[source_start..source_end].find(['\n', '\r']) {
            let first_line = &text[source_start..source_start + line_end];
            if CAPTURE_PROVIDER_TOKEN.is_match(first_line) {
                let source_end = source_start + line_end;
                let token = NOISE.replace_all(first_line, "");
                output.push_str(&text[consumed..source_start]);
                output.push_str(&mask_capture_token(&token));
                consumed = source_end;
                continue;
            }
        }
        if text.as_bytes().get(source_end) == Some(&b'=') {
            continue;
        }
        output.push_str(&text[consumed..source_start]);
        output.push_str(&mask_capture_token(token.as_str()));
        consumed = source_end;
    }
    output.push_str(&text[consumed..]);
    CAPTURE_PROVIDER_TOKEN
        .replace_all(&output, |groups: &regex::Captures<'_>| {
            if groups[0].len() < 18 && output[groups.get(0).unwrap().end()..].starts_with("...") {
                groups[0].to_owned()
            } else {
                mask_capture_token(&groups[0])
            }
        })
        .into_owned()
}

fn redact_capture_assignments(text: &str) -> String {
    static UPPER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)\b([A-Z0-9_]+)[ \t]*=[ \t]*(?:"([^\s"]+)"|'([^\s']+)'|(\S+))"#).unwrap()
    });
    static LOWER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\b([a-z0-9_]+_(?:key|pass|pw|token|secret|password|passwd|credential|auth))[ \t]*=[ \t]*(?:"([^\s"]+)"|'([^\s']+)'|(\S+))"#).unwrap()
    });
    static CONFIG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?m)^[ \t]*([A-Za-z0-9_.-]+)[ \t]*=[ \t]*(?:"([^\s"]+)"|'([^\s']+)'|([^\s&]+))"#,
        )
        .unwrap()
    });
    static YAML: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?m)^([ \t]*)([A-Za-z0-9_.-]+)(:[ \t]*)(?:"([^"\r\n]+)"|'([^'\r\n]+)'|([^\s&'"]+))"#,
        )
        .unwrap()
    });
    let equals = |groups: &regex::Captures<'_>| {
        let (quote, value) = capture_assignment_value(groups, 2);
        let key = &groups[1];
        let azure_field = ["AccountKey", "SharedAccessKey", "SharedAccessSignature"]
            .iter()
            .any(|field| key.eq_ignore_ascii_case(field));
        if !azure_field && should_mask_capture_assignment(key, value) {
            let indent =
                &groups[0][..groups.get(1).unwrap().start() - groups.get(0).unwrap().start()];
            format!("{indent}{key}={quote}{}{quote}", mask_capture_token(value))
        } else {
            groups[0].to_owned()
        }
    };
    let cookies = capture_specialized::cookie_assignments(text);
    let upper = UPPER.replace_all(&cookies, equals);
    let config = if text.contains("://") {
        upper
    } else {
        let lower = LOWER.replace_all(&upper, equals);
        let config = CONFIG.replace_all(&lower, equals);
        Cow::Owned(
            YAML.replace_all(&config, |groups: &regex::Captures<'_>| {
                let (quote, value) = capture_assignment_value(groups, 4);
                if should_mask_capture_assignment(&groups[2], value) {
                    format!(
                        "{}{}{}{quote}{}{quote}",
                        &groups[1],
                        &groups[2],
                        &groups[3],
                        mask_capture_token(value)
                    )
                } else {
                    groups[0].to_owned()
                }
            })
            .into_owned(),
        )
    };
    capture_specialized::structured_fields(&config)
}

fn capture_assignment_value<'a>(
    groups: &regex::Captures<'a>,
    start: usize,
) -> (&'static str, &'a str) {
    if let Some(value) = groups.get(start) {
        ("\"", value.as_str())
    } else if let Some(value) = groups.get(start + 1) {
        ("'", value.as_str())
    } else {
        (
            "",
            groups
                .get(start + 2)
                .expect("assignment value matched")
                .as_str(),
        )
    }
}

fn is_capture_marker(value: &str) -> bool {
    value == "***" || value.starts_with("«redacted") || value == HIDDEN || value == "[REDACTED"
}

fn should_mask_capture_assignment(key: &str, value: &str) -> bool {
    if is_capture_marker(value)
        || ["os.getenv", "os.environ", "process.env", "$ENV{"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "none" | "null"
        )
    {
        return false;
    }
    let key = capture_key_words(key);
    let words: Vec<_> = key.split('_').collect();
    let password = words
        .iter()
        .any(|word| matches!(*word, "password" | "passwd" | "pw" | "pass"));
    if !password && value.starts_with(['$', '~', '/']) {
        return false;
    }
    let strong = password
        || words.iter().any(|word| {
            matches!(
                *word,
                "apikey" | "auth" | "secret" | "credential" | "credentials" | "bearer"
            )
        })
        || words.windows(2).any(|pair| {
            matches!(
                pair,
                [
                    "api" | "auth" | "access" | "refresh" | "session" | "id" | "bearer",
                    "key" | "token"
                ]
            )
        });
    strong
        || (words.iter().any(|word| matches!(*word, "key" | "token"))
            && opaque_capture_value(value))
}

fn opaque_capture_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 16 && bytes.iter().all(u8::is_ascii_hexdigit))
        || (bytes.len() >= 20
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_./+=-".contains(byte)))
        || (bytes.len() >= 12
            && [
                u8::is_ascii_lowercase,
                u8::is_ascii_uppercase,
                u8::is_ascii_digit,
            ]
            .iter()
            .filter(|class| bytes.iter().any(*class))
            .count()
                >= 2)
}

fn capture_key_words(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len());
    let mut lowercase = false;
    for c in key.chars() {
        if lowercase && c.is_ascii_uppercase() {
            normalized.push('_');
        }
        normalized.push(if matches!(c, '-' | '.') {
            '_'
        } else {
            c.to_ascii_lowercase()
        });
        lowercase = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    normalized
}

fn redact_capture_urls(text: &str) -> String {
    static URL_USERINFO: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"//[^/\s?#@"]+@"#).unwrap());
    static URL_PARAMETER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"([?&;#])([A-Za-z0-9_.~+%\-]+)=([^&#;\s"'<>]*(?:'[A-Za-z0-9_~+%\-][^&#;\s"'<>]*)*)"#,
        )
        .unwrap()
    });
    let params = URL_PARAMETER.replace_all(text, |groups: &regex::Captures<'_>| {
        if secret_parameter(&groups[2]) {
            format!("{}{}=***", &groups[1], &groups[2])
        } else {
            groups[0].to_owned()
        }
    });
    URL_USERINFO.replace_all(&params, "//***:***@").into_owned()
}

fn secret_parameter(name: &str) -> bool {
    let mut name = name.to_owned();
    for _ in 0..3 {
        if !name.contains('%') {
            break;
        }
        let mut decoded = String::with_capacity(name.len());
        let mut remaining = name.as_str();
        while let Some((head, escaped)) = remaining.split_once('%') {
            decoded.push_str(head);
            let Some(byte) = escaped
                .get(..2)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            else {
                return false;
            };
            decoded.push(char::from(byte));
            remaining = &escaped[2..];
        }
        decoded.push_str(remaining);
        name = decoded;
    }
    let name = capture_key_words(&name);
    let key = name.strip_suffix("_value").unwrap_or(&name);
    matches!(
        key,
        "auth"
            | "jwt"
            | "session"
            | "code"
            | "signature"
            | "x_amz_signature"
            | "x_goog_signature"
            | "sig"
            | "key"
            | "bearer"
            | "key_material"
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
    .any(|suffix| key.ends_with(suffix))
        || (key.ends_with("_key")
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
            }))
}

#[cfg(test)]
mod capture_tests {
    use super::redact_capture;

    #[test]
    fn preserves_plain_output_including_controls_and_replacement_characters() {
        let text =
            "hello\0world\x01\x1b[31mred\x1b[0m\x07\x08\t\x0c\x7f\r\nbad:\u{fffd}\n日本語 🐕";
        assert_eq!(redact_capture(text, &[]), text);
    }

    #[test]
    fn follows_pinned_stock_display_masks_for_provider_tokens_and_urls() {
        let input = concat!(
            "key=sk-or-v1-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAffff\n",
            "tvly-123456789abc ghp_1234567890abcdefghijklmnop eyJabcdefghijk.1234.5678\n",
            "https://test-user:test-pass@example.test/path?ac%63ess_token=fake-query&q=keep#code=fake-code\n",
            "//name@example.test/ https://example.test/?token=&access-token=fake&userToken=opaque\n",
            "Authorization: Bearer ABCDEF0123456789XYZ\n",
            "x ghp_12345\x1b[31m67890abcdefghi y\n",
            "token_count=12 α🐕"
        );
        assert_eq!(
            redact_capture(input, &[]),
            concat!(
                "key=sk-or-...ffff\n",
                "*** ghp_12...mnop eyJabc...5678\n",
                "https://***:***@example.test/path?ac%63ess_token=***&q=keep#code=***\n",
                "//***:***@example.test/ https://example.test/?token=***&access-token=***&userToken=***\n",
                "Authorization: Bearer ABCDEF...9XYZ\n",
                "x ghp_12...fghi y\n",
                "token_count=12 α🐕"
            )
        );
    }

    #[test]
    fn matches_stock_for_serialized_keys_headers_and_webhooks() {
        use base64::Engine as _;
        // A zero snowflake and repeated dummy bytes can never authenticate a bot.
        let zero_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([b'0'; 18]);
        let discord = format!(
            "bot {zero_id}.sample.{}\nmfa.{}\nSomeVeryLongClassName.Config.AnotherLongAttributeName\n",
            "x".repeat(27),
            "Z".repeat(84),
        );
        let fixtures = [
            (
                "header_truncated",
                "{\"X-Api-Key\": \"opaque-synthetic-key-12345\n{\"Cookie\": \"sid=opaque-synthetic-cookie-12345\n",
                "{\"X-Api-Key\": \"opaque...2345\n{\"Cookie\": \"sid=op...2345\n",
            ),
            (
                "cookie_bare",
                "info Set-Cookie: ABCDEF0123456789abcdefghijklmnop; secure\ninfo Set-Cookie: ordinary\n",
                "info Set-Cookie: ***; secure\ninfo Set-Cookie: ordinary\n",
            ),
            (
                "discord_bot",
                discord.as_str(),
                "bot MDAwMD...xxxx\nmfa.ZZ...ZZZZ\nSomeVeryLongClassName.Config.AnotherLongAttributeName\n",
            ),
            (
                "serialized_private_key",
                "client-key-data: LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0tClFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVEKLS0tLS1FTkQgUFJJVkFURSBLRVktLS0tLQo=\ncontent: LS0tLS1CRUdJTiBQVUJMSUMgS0VZLS0tLS0KUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUQ==",
                "client-key-data: [REDACTED PRIVATE KEY]\ncontent: LS0tLS1CRUdJTiBQVUJMSUMgS0VZLS0tLS0KUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUQ==",
            ),
            (
                "pgp_pem",
                "-----BEGIN PGP PRIVATE KEY BLOCK-----\nnot-a-real-key\n-----END PGP PRIVATE KEY BLOCK-----\n-----BEGIN PRIVATE KEY-----\nQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ\n-----END PRIVATE KEY-----\n",
                "[REDACTED PRIVATE KEY]\n[REDACTED PRIVATE KEY]\n",
            ),
            (
                "truncated_pem",
                "-----BEGIN PRIVATE KEY-----\nUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUU==\nregular output\n",
                "[REDACTED PRIVATE KEY]\nregular output\n",
            ),
            (
                "structured_key",
                "{\"signing_key\":\"opaque-signing-synthetic-0123456789\",\"privateKey\":\"synthetic-private-key\",\"passwordChanged\":\"2026-11-23T20:03:44+00:00\",\"api_key_value\":\"synthetic-abcdefgh0123456789\",\"apiKey\":\"example\",\"token\":\"local\",\"redact_secrets\":false}",
                "{\"signing_key\": \"opaque...6789\",\"privateKey\": \"synthe...-key\",\"passwordChanged\":\"2026-11-23T20:03:44+00:00\",\"api_key_value\": \"synthe...6789\",\"apiKey\":\"***\",\"token\":\"local\",\"redact_secrets\":false}",
            ),
            (
                "structured_escaped",
                "{\"record\":\"{\\\"client_secret\\\":\\\"abcdefghijklmnopqrst0123\\\",\\\"password\\\":\\\"test credential\\\"}\"}",
                "{\"record\":\"{\\\"client_secret\\\": \\\"abcdef...0123\\\",\\\"password\\\": \\\"***\\\"}\"}",
            ),
            (
                "python_repr",
                "{'API_KEY': 'synthetic-short', 'AuthToken': \"shared-test-value\", 'Authorization': 'Basic synthetic-value', 'token_count': 5}",
                "{'API_KEY': '***', 'AuthToken': \"***\", 'Authorization': '***', 'token_count': 5}",
            ),
            (
                "unicode_mask",
                "api_key: \"éé有限😀e\u{200b}\u{202e}abcdefghiſ末尾🔥\"\nfolder: ö/日本語/\u{200d}\u{200b}\u{202e}\u{0000}\n",
                "api_key: \"éé有限😀e...ſ末尾🔥\"\nfolder: ö/日本語/\u{200d}\u{200b}\u{202e}\u{0000}\n",
            ),
            (
                "cookies_raw",
                "Cookie: session=synthetic-opaque; auth=fixture-session\n< Set-Cookie: session=synthetic-cookie; Path=/; HttpOnly\nprose cookie: delicious\ninfo Cookie: sid=demo123; theme=dark ended",
                "Cookie: ***\n< Set-Cookie: ***\nprose cookie: delicious\ninfo Cookie: *** ended",
            ),
            (
                "cookies_curl",
                "curl -H 'Cookie: sid=synthetic-cookie; theme=dark' -H \"Set-Cookie: sid=fixture-1234\" -H Cookie:short=opaque --cookie 'sid=synthetic-secret' https://example.test/",
                "curl -H 'Cookie: ***' -H \"Set-Cookie: ***\" -H Cookie:*** --cookie 'sid=synthetic-secret' https://example.test/",
            ),
            (
                "cookies_json",
                "{\"Cookie\": \"session=synthetic-cookie; theme=dark\", \"Set-Cookie\": \"opt=fixture\", \"ordinary\": \"unrelated\", \"cookie_policy\":\"strict\", \"X-Api-Key\": \"opaque-synthetic-12345\"}",
                "{\"Cookie\": \"sessio...dark\", \"Set-Cookie\": \"***\", \"ordinary\": \"unrelated\", \"cookie_policy\":\"strict\", \"X-Api-Key\": \"opaque...2345\"}",
            ),
            (
                "cookies_env",
                "SESSION_COOKIE='synthetic-short'\ncookie=chocolate\nCOOKIE='sid=synthetic-credential; opt=fixture'\nprefs_cookie=demo\n?cookie=public\n",
                "SESSION_COOKIE='***'\ncookie=chocolate\nCOOKIE='sid=sy...ture'\nprefs_cookie=***\n?cookie=public\n",
            ),
            (
                "webhooks",
                "https://hooks.slack.com/services/T123/B456/shortKey1234\nhttps://hooks.slack-gov.com.:8443/workflows/T123/B456/42/abcdefghijklmnopqrstuvwxyz\nhttps://discordapp.com/api/v10/webhooks/12345/AAAAAAAAAAAAAAAAbbbbbbbbbbb\nhttps://office-example.webhook.office.com/webhookb2/abcdefghij@klmnopqrst/path\nhttps://example.logic.azure.com:443/my/workflows/run?api-version=2020&sig=aaaaaaaaaaaabbbbbbbbbb\nhttps://example.test/hooks/abcdefghijklmnopqrstuvwxyz",
                "https://hooks.slack.com/services/T123/B456/***\nhttps://hooks.slack-gov.com.:8443/workflows/T123/B456/42/abcdef...wxyz\nhttps://discordapp.com/api/v10/webhooks/12345/AAAAAA...bbbb\nhttps://office-example.webhook.office.com/webhookb2/abcdef...path\nhttps://example.logic.azure.com:443/my/workflows/run?api-version=2020&sig=***\nhttps://example.test/hooks/abcdefghijklmnopqrstuvwxyz",
            ),
            (
                "forms",
                "access_token=synthetic-value&mode=readonly&X-Amz-Signature=another-synthetic-value",
                "access_token=***",
            ),
            (
                "cli",
                "curl --password 'very long mock password' --token=someopaque0123456789 --api-key=$EXAMPLE_KEY https://example.test/",
                "curl --password very l...word --token=someop...6789 --api-key=$EXAMPLE_KEY https://example.test/",
            ),
            (
                "optional_force_on",
                "my key sk-or-v1-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAffff",
                "my key sk-or-...ffff",
            ),
        ];
        let mut differences = Vec::new();
        for (name, input, expected) in fixtures {
            let actual = redact_capture(input, &[]);
            if actual != expected {
                differences.push(format!(
                    "{name}:\n  actual:   {actual:?}\n  expected: {expected:?}"
                ));
            }
        }
        assert!(differences.is_empty(), "{}", differences.join("\n"));
    }

    #[test]
    fn matches_stock_for_connection_strings_and_signin_credentials() {
        let fixtures = [
            (
                "zhipu_api_key",
                "0123456789abcdef0123456789abcdef.SyntheticSecret0123456789 and (abcdef0123456789abcdef0123456789.ABCDEFGHIJKLM0123)\n0123456789abcdef0123456789abcdef.sqlite3\nname-0123456789abcdef0123456789abcdef.SyntheticSecret0123456789\n",
                "012345...6789 and (abcdef...0123)\n0123456789abcdef0123456789abcdef.sqlite3\nname-0123456789abcdef0123456789abcdef.SyntheticSecret0123456789\n",
            ),
            (
                "webapp_signin",
                "https://fixture.invalid/app#hermes-session=synthetic-0123456789-abcdefghijklmnop&theme=dark\nhttps://fixture.invalid/webapp/launch#profile=test&ticket=abc0123456789syntheticUVWXYZ\n/webapp/launch#ticket=long-fixture-value-123456789012\nhttps://fixture.invalid/app#hermes-session=short\nhttps://fixture.invalid/normal#ticket=unrelated-ticket0123456789\nhttps://fixture.invalid/oauth?code=oauth-fixture-secret&state=public\n",
                "https://fixture.invalid/app#hermes-session=***&theme=dark\nhttps://fixture.invalid/webapp/launch#profile=test&ticket=***\n/webapp/launch#ticket=***\nhttps://fixture.invalid/app#hermes-session=short\nhttps://fixture.invalid/normal#ticket=unrelated-ticket0123456789\nhttps://fixture.invalid/oauth?code=***&state=public\n",
            ),
            (
                "azure_account_key",
                "DefaultEndpointsProtocol=https;AccountName=fixture;AccountKey=YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZg==;EndpointSuffix=example.test\nSharedAccessKey=YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZg==;EntityPath=test\nAccountKey = AbCdeFGH0123456789;AccountName=fixture\nAccountKey={ACCOUNT_KEY}\nAccountKey=123456\n",
                "DefaultEndpointsProtocol=https;AccountName=fixture;AccountKey=YWJjZG...Zg==;EndpointSuffix=example.test\nSharedAccessKey=YWJjZG...Zg==;EntityPath=test\nAccountKey = AbCdeF...6789;AccountName=fixture\nAccountKey={ACCOUNT_KEY}\nAccountKey=123456\n",
            ),
            (
                "azure_sas",
                "HostName=fixture.invalid;SharedAccessSignature=sv=2025&sr=c&sig=synthetic-signature0123456789&se=2028;Endpoint=fixture.invalid\nSharedAccessSignature=sr=c&sig=short;EntityPath=fixture\n",
                "HostName=fixture.invalid;SharedAccessSignature=sv=202...6789&se=2028;Endpoint=fixture.invalid\nSharedAccessSignature=***;EntityPath=fixture\n",
            ),
            (
                "libpq_conninfo",
                "psql \"host=fixture.invalid user=fixture password=synthetic-secret dbname=fixture\"\npsql \"host=fixture.invalid user=fixture password='synthetic multiword value' dbname=fixture\"\nhost=fixture.invalid password=a;b dbname=fixture\nhost=fixture.invalid password=masked\\ space port=5432\nhost=fixture.invalid password='' dbname=fixture\nthe password= option for code dbname=fixture\npassword=literal\n",
                "psql \"host=fixture.invalid user=fixture password=*** dbname=fixture\"\npsql \"host=fixture.invalid user=fixture password='***' dbname=fixture\"\nhost=fixture.invalid password=*** dbname=fixture\nhost=fixture.invalid password=*** port=5432\nhost=fixture.invalid password='***' dbname=fixture\nthe password= option for code dbname=fixture\npassword=***\n",
            ),
            (
                "kv_semicolon",
                "Host=fixture.invalid;Username=fixture;Password=plain text credential;Database=fixture\nServer=fixture.invalid;PWD={fake;password};Database=fixture\nServer=fixture.invalid;Password='double''quote secret';Database=fixture\nConnection settings host=fixture.invalid without a key\n",
                "Host=fixture.invalid;Username=fixture;Password=***;Database=fixture\nServer=fixture.invalid;PWD={***};Database=fixture\nServer=fixture.invalid;Password=***'***';Database=fixture\nConnection settings host=fixture.invalid without a key\n",
            ),
        ];
        let mut differences = Vec::new();
        for (name, input, expected) in fixtures {
            let actual = redact_capture(input, &[]);
            if actual != expected {
                differences.push(format!("{name}:\n  actual:   {actual:?}\n  expected: {expected:?}"));
            }
        }
        assert!(differences.is_empty(), "{}", differences.join("\n"));
    }

    #[test]
    fn protected_provider_values_use_stock_masks_and_opaque_literals_are_fully_hidden() {
        let provider = "sk-or-v1-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAffff";
        let opaque = "native-gateway-auth-test-0123456789";
        let text = format!("key={provider}\nembedded:{provider}/{opaque}\nexact-protected-long");
        assert_eq!(
            redact_capture(&text, &[provider, opaque, "exact-protected", "exact-protected-long"]),
            "key=sk-or-...ffff\nembedded:sk-or-...ffff/[REDACTED]\n[REDACTED]"
        );
    }
}

fn sensitive_name(key:&str)->bool{
    let key=key.to_ascii_lowercase().replace(['-','.'],"_");
    ["api_key","apikey","password","passwd","client_secret","access_token","refresh_token","private_key","auth_token"].iter().any(|field|key==*field||key.ends_with(&format!("_{field}")))
}
fn redact_value(value:&mut Value){
    match value {
        Value::String(s)=>redact(s),
        Value::Array(a)=>a.iter_mut().for_each(redact_value),
        Value::Object(obj)=>for (key,v) in obj{
            if sensitive_name(key)&&!v.is_null(){*v=Value::String(HIDDEN.into());}else{redact_value(v);}
        },
        _=>{}
    }
}
pub(crate) fn redact_messages(messages:&mut [Value]){
    for message in messages {
        let tool=message["role"]=="tool";
        match &mut message["content"] {
            Value::String(s) if tool=>if let Ok(mut data)=serde_json::from_str::<Value>(s){
                redact_value(&mut data);*s=data.to_string();
            }else{redact(s)},
            Value::String(s)=>redact(s),
            Value::Array(parts)=>for p in parts {if let Some(Value::String(s))=p.get_mut("text"){redact(s)}},
            _=>{}
        }
        if let Some(calls)=message.get_mut("tool_calls").and_then(Value::as_array_mut){
            for call in calls {
                if let Some(Value::String(args))=call["function"].get_mut("arguments") && let Ok(mut data)=serde_json::from_str::<Value>(args){
                    redact_value(&mut data);*args=data.to_string();
                }
            }
        }
    }
}
