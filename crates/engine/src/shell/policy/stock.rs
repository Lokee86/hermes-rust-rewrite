//! Stock's pinned regex rules are a second refusal layer over the native grammar.
//! The grammar establishes admissibility because the stock classifier also has
//! structural scanner branches that the regex tables cannot represent.
use super::{
    Shell,
    options::{InterpreterSource, Options, ReadTool},
};
use fancy_regex::Regex;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    sync::{LazyLock, OnceLock},
};
use unicode_normalization::UnicodeNormalization;

#[derive(Deserialize)]
struct Generated {
    interpreter_options: Vec<InterpreterSource>,
    read_tool_options: BTreeMap<String, ReadTool>,
    hardline: Vec<GeneratedRule>,
    dangerous: Vec<GeneratedRule>,
}

#[derive(Deserialize)]
struct GeneratedRule(String, u32, bool, Vec<String>);

struct Rules {
    options: Options,
    hardline: Vec<Rule>,
    dangerous: Vec<Rule>,
}

struct Rule {
    source: GeneratedRule,
    regex: OnceLock<Option<Regex>>,
}

static RULES: LazyLock<Option<Rules>> = LazyLock::new(|| {
    let sources: Generated = serde_json::from_str(include_str!("stock_rules.json")).ok()?;
    Some(Rules {
        options: Options::new(sources.interpreter_options, sources.read_tool_options)?,
        hardline: load(sources.hardline),
        dangerous: load(sources.dangerous),
    })
});

static IFS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\$\{IFS\b[^}]*\}|\$IFS\b").unwrap());

fn load(sources: Vec<GeneratedRule>) -> Vec<Rule> {
    sources
        .into_iter()
        .map(|source| Rule {
            source,
            regex: OnceLock::new(),
        })
        .collect()
}

fn compile(source: &str, flags: u32) -> Option<Regex> {
    let mut pattern = String::from("(?");
    for (bit, flag) in [(2, 'i'), (8, 'm'), (16, 's')] {
        if flags & bit != 0 {
            pattern.push(flag);
        }
    }
    if pattern.len() == 2 {
        pattern.clear();
    } else {
        pattern.push(')');
    }
    // Python's strict end-of-string anchor is spelled differently by Rust regex.
    pattern.push_str(&source.replace(r"\Z", r"\z"));
    Regex::new(&pattern).ok()
}

pub(super) fn program_options() -> Option<&'static Options> {
    RULES.as_ref().map(|rules| &rules.options)
}

pub(super) fn allows(command: &str, stages: &[&str], shell: Shell) -> bool {
    let Some(rules) = &*RULES else {
        return false;
    };
    std::iter::once(command)
        .chain(stages.iter().copied().filter(|stage| *stage != command))
        .all(|part| {
            let normalized = normalize(part);
            allows_variant(part, rules, shell)
                && (normalized == part || allows_variant(&normalized, rules, shell))
        })
}

fn allows_variant(command: &str, rules: &Rules, shell: Shell) -> bool {
    let masked = mask_quotes(command, shell);
    command_passes(
        &rules.hardline,
        &command.to_lowercase(),
        &masked.to_lowercase(),
    ) && command_passes(
        &rules.dangerous,
        &lower_preserving_flags(command),
        &lower_preserving_flags(&masked),
    )
}

fn command_passes(rules: &[Rule], raw: &str, masked: &str) -> bool {
    let raw_ascii = raw.is_ascii().then(|| raw.to_ascii_lowercase());
    let masked_ascii = masked.is_ascii().then(|| masked.to_ascii_lowercase());
    rules.iter().all(|rule| {
        let GeneratedRule(source, flags, mask_quotes, literals) = &rule.source;
        let (text, ascii) = if *mask_quotes {
            (masked, &masked_ascii)
        } else {
            (raw, &raw_ascii)
        };
        if !literals.is_empty()
            && ascii
                .as_ref()
                .is_some_and(|text| literals.iter().all(|literal| !text.contains(literal)))
        {
            return true;
        }
        let regex = rule.regex.get_or_init(|| compile(source, *flags));
        regex
            .as_ref()
            .is_some_and(|regex| matches!(regex.is_match(text), Ok(false)))
    })
}

fn mask_quotes(text: &str, shell: Shell) -> String {
    let mut quote = None;
    text.chars()
        .map(|ch| match quote {
            Some(delimiter) if delimiter == ch => {
                quote = None;
                ch
            }
            Some(_) => ' ',
            None if ch == '"' || (ch == '\'' && shell == Shell::Posix) => {
                quote = Some(ch);
                ch
            }
            None => ch,
        })
        .collect()
}

fn normalize(command: &str) -> String {
    // The positive grammar has already refused executable expansions, ANSI-C quotes,
    // control escapes and absolute file redirects.
    let unicode: String = command.nfkc().collect();
    let joined = unicode.replace("\\\r\n", "").replace("\\\n", "");
    let mut chars = joined.chars();
    let mut unescaped = String::with_capacity(joined.len());
    while let Some(ch) = chars.next() {
        let ch = if ch == '\\' {
            chars.next().unwrap_or(ch)
        } else {
            ch
        };
        unescaped.push(ch);
    }
    let unescaped = unescaped.replace("''", "").replace("\"\"", "");
    IFS.replace_all(&unescaped, " ").into_owned()
}

fn lower_preserving_flags(command: &str) -> String {
    let mut lower = String::with_capacity(command.len());
    for word in command.split_inclusive(char::is_whitespace) {
        if word.starts_with('-') {
            lower.push_str(word);
        } else {
            lower.push_str(&word.to_lowercase());
        }
    }
    lower
}
