//! Argument ownership for the pinned stock interpreter and read-tool rules.
use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
pub(super) struct InterpreterSource(pub String, pub String, pub Vec<String>, pub Vec<String>);

#[derive(Deserialize)]
pub(super) struct ReadTool(pub Vec<String>, pub Vec<String>, pub String);

pub(super) struct Options {
    interpreters: Vec<Interpreter>,
    read_tools: BTreeMap<String, ReadTool>,
}

struct Interpreter {
    family: String,
    name: Regex,
    exec: Vec<String>,
    argument: Vec<String>,
}

impl Options {
    pub(super) fn new(
        interpreters: Vec<InterpreterSource>,
        read_tools: BTreeMap<String, ReadTool>,
    ) -> Option<Self> {
        let interpreters = interpreters
            .into_iter()
            .map(|InterpreterSource(family, name, exec, argument)| {
                Some(Interpreter {
                    family,
                    name: Regex::new(&format!(r"\A(?:{name})\z")).ok()?,
                    exec,
                    argument,
                })
            })
            .collect::<Option<_>>()?;
        Some(Self {
            interpreters,
            read_tools,
        })
    }

    pub(super) fn executes_program(&self, name: &str, args: &[&str]) -> bool {
        self.interpreters
            .iter()
            .find(|interpreter| interpreter.name.is_match(name))
            .is_some_and(|interpreter| interpreter.exec_option(args))
            || self
                .read_tools
                .get(name)
                .is_some_and(|tool| tool.exec_option(name, args))
    }

    pub(super) fn read_tool(&self, name: &str) -> Option<&ReadTool> {
        self.read_tools.get(name)
    }
}

impl Interpreter {
    fn exec_option(&self, args: &[&str]) -> bool {
        let powershell = self.family == "powershell";
        let mut args = args.iter().copied();
        while let Some(token) = args.next() {
            if token == "--" || (!powershell && !token.starts_with('-')) {
                return self.family == "deno" && token.eq_ignore_ascii_case("eval");
            }
            let (option, payload) = split_option(token);
            let lowercase;
            let option = if powershell {
                lowercase = option.to_ascii_lowercase();
                lowercase.as_str()
            } else {
                option
            };
            if self.exec.iter().any(|flag| flag == option) {
                return true;
            }
            let has_attached_argument = self.argument.iter().any(|flag| {
                flag.starts_with('-')
                    && !flag.starts_with("--")
                    && option.len() > flag.len()
                    && option.starts_with(flag)
            });
            if !powershell
                && !option.starts_with("--")
                && option.len() > 2
                && !has_attached_argument
                && option.chars().skip(1).any(|ch| {
                    self.exec.iter().any(|flag| {
                        let mut chars = flag.chars();
                        chars.next() == Some('-')
                            && chars.next() == Some(ch)
                            && chars.next().is_none()
                    })
                })
            {
                return true;
            }
            if payload.is_none() && self.argument.iter().any(|flag| flag == option) {
                args.next();
            }
        }
        false
    }
}

impl ReadTool {
    fn exec_option(&self, name: &str, args: &[&str]) -> bool {
        let mut args = args.iter().copied();
        while let Some(token) = args.next() {
            if token == "--" {
                break;
            }
            let (option, mut payload) = split_option(token);
            let mut matched = self.0.iter().any(|flag| flag == option);
            if name == "man"
                && (token.starts_with("-P") || token.starts_with("-H"))
                && token.len() > 2
            {
                matched = true;
                payload = Some(&token[2..]);
            }
            if matched {
                if payload
                    .or_else(|| args.next())
                    .is_some_and(|value| !value.is_empty())
                {
                    return true;
                }
            } else if self.option_takes_following_argument(option, payload, token) {
                args.next();
            }
        }
        false
    }

    fn option_takes_following_argument(
        &self,
        option: &str,
        payload: Option<&str>,
        token: &str,
    ) -> bool {
        if self.long_takes_argument(option) && payload.is_none() {
            return true;
        }
        if let Some(shorts) = token
            .strip_prefix('-')
            .filter(|flags| !flags.starts_with('-'))
        {
            let owner = shorts
                .char_indices()
                .find(|&(_, ch)| self.short_takes_argument(ch));
            return owner.is_some_and(|(index, ch)| index + ch.len_utf8() == shorts.len());
        }
        false
    }

    pub(super) fn long_takes_argument(&self, option: &str) -> bool {
        self.1.iter().any(|flag| flag == option)
    }

    pub(super) fn short_takes_argument(&self, flag: char) -> bool {
        self.2.contains(flag)
    }
}

pub(super) fn split_option(token: &str) -> (&str, Option<&str>) {
    match token.split_once('=') {
        Some((option, payload)) => (option, Some(payload)),
        None => (token, None),
    }
}
