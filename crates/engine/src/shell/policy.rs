//! The local subset the composer can classify before invoking the native shell.
//!
//! Stock Hermes runs the Python approval detector. This implementation admits specific
//! command forms and shell operators whose interpretation is known on each platform.
//! Capture must select the native interpreter and provide a trusted executable search path.

use std::{ffi::OsString, path::Path};

#[path = "policy/cmd_control.rs"]
mod cmd_control;
#[path = "policy/git.rs"]
mod git;
#[cfg(windows)]
pub(super) use git::worktree_read;
#[path = "policy/options.rs"]
mod options;
#[path = "policy/queries.rs"]
mod queries;

#[path = "policy/path.rs"]
mod path;
use path::{local_read_path, read_path};
#[path = "policy/stock.rs"]
mod stock;

const MAX_COMMAND_LENGTH: usize = 16_384;

pub(super) struct Context<'a> {
    pub(super) cwd: &'a Path,
    pub(super) environment: &'a [(OsString, OsString)],
    pub(super) trusted_programs: &'a [&'a str],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shell {
    Posix,
    Cmd,
}

pub(super) fn allows_with_context(command: &str, context: &Context<'_>) -> bool {
    allows_for(command, native_shell(), Some(context))
}

pub(super) fn pip_query(command: &str, context: &Context<'_>) -> Option<std::path::PathBuf> {
    let shell = native_shell();
    if !valid_input(command, shell) {
        return None;
    }
    let mut source = Source {
        rest: command,
        shell,
        literal_git: false,
    };
    let Token::Word(name, quote) = source.next().ok()?? else {
        return None;
    };
    if expansion_present(name, quote, shell)
        || !matches!(
            trusted_external(name, shell, Some(context)),
            Some(External::Python)
        )
    {
        return None;
    }
    for expected in ["-m", "pip", "--version"] {
        if !matches!(source.next().ok()??, Token::Word(word, None) if word == expected) {
            return None;
        }
    }
    if source.next().ok()?.is_some() || !stock::allows(command, &[command], shell) {
        return None;
    }

    let executable = context.trusted_programs.iter().find_map(|program| {
        let path = Path::new(program);
        let basename = program_basename(program, shell);
        let matches = match shell {
            Shell::Posix => *program == name || basename == name,
            Shell::Cmd => {
                program.eq_ignore_ascii_case(name)
                    || basename.eq_ignore_ascii_case(name)
                    || basename
                        .to_ascii_lowercase()
                        .strip_suffix(".exe")
                        .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
            }
        };
        (path.is_absolute() && matches).then_some(path)
    })?;
    let resolved = executable.canonicalize().ok()?;
    // Preserve the lexical executable path so a selected virtualenv stays selected.
    (!resolved.starts_with(context.cwd)).then(|| executable.to_owned())
}

#[cfg(windows)]
pub(super) fn echo_findstr<'a>(
    command: &'a str,
    context: &Context<'_>,
) -> Option<(&'a str, &'a str, &'a str)> {
    if !valid_input(command, Shell::Cmd) {
        return None;
    }
    let rest = command.strip_prefix("echo \"")?;
    let end = rest.find('"')?;
    let literal = &rest[..end];
    let after_quote = &rest[end + 1..];
    let trailing_spaces =
        &after_quote[..after_quote.bytes().take_while(|byte| *byte == b' ').count()];
    if trailing_spaces.is_empty() {
        return None;
    }
    let pattern = after_quote[trailing_spaces.len()..].strip_prefix("| findstr ")?;
    if !literal.is_ascii()
        || literal.contains(['%', '^', '!'])
        || literal.chars().any(char::is_control)
        || pattern.is_empty()
        || !pattern
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }

    let echo = [("echo", None), (literal, Some('"'))];
    let findstr = [("findstr", None), (pattern, None)];
    (allowed_command(&echo, Shell::Cmd, Some(context))
        && allowed_command(&findstr, Shell::Cmd, Some(context))
        && stock::allows(command, &[command], Shell::Cmd))
    .then_some((literal, trailing_spaces, pattern))
}

#[cfg(windows)]
pub(super) fn more_operand(command: &str) -> Option<&str> {
    if !valid_input(command, Shell::Cmd) {
        return None;
    }
    let mut source = Source {
        rest: command,
        shell: Shell::Cmd,
        literal_git: false,
    };
    let Token::Word(program, None) = source.next().ok()?? else {
        return None;
    };
    if !(program.eq_ignore_ascii_case("more") || program.eq_ignore_ascii_case("more.com")) {
        return None;
    }
    let Token::Word(path, _) = source.next().ok()?? else {
        return None;
    };
    if source.next().ok()?.is_some()
        || !relative_path(path, Shell::Cmd, false)
        || !stock::allows(command, &[command], Shell::Cmd)
    {
        return None;
    }
    Some(path)
}

#[cfg(windows)]
pub(super) struct FindstrSearch<'a> {
    pub(super) args: Vec<&'a str>,
    pub(super) operand: &'a str,
    pub(super) recursive: bool,
}

#[cfg(windows)]
pub(super) fn findstr_search(command: &str) -> Option<FindstrSearch<'_>> {
    if !valid_input(command, Shell::Cmd) || command.contains(['%', '!']) {
        return None;
    }
    let mut source = Source {
        rest: command,
        shell: Shell::Cmd,
        literal_git: false,
    };
    let Token::Word(program, None) = source.next().ok()?? else {
        return None;
    };
    if !["findstr", "findstr.exe"]
        .iter()
        .any(|name| program.eq_ignore_ascii_case(name))
        || !source.rest.starts_with([' ', '\t'])
    {
        return None;
    }

    let mut words = Vec::new();
    while let Some(token) = source.next().ok()? {
        let Token::Word(word, quote) = token else {
            return None;
        };
        if !source.rest.is_empty() && !source.rest.starts_with(char::is_whitespace) {
            return None;
        }
        words.push((word, quote));
    }
    let ((operand, _), words) = words.split_last()?;
    let mut args = Vec::new();
    let mut recursive = false;
    let mut pattern_seen = false;
    for &(word, quote) in words {
        if pattern_seen {
            return None;
        }
        if quote.is_none() && word.eq_ignore_ascii_case("/s") {
            recursive = true;
        } else {
            args.push(word);
            pattern_seen = quote.is_some()
                || !FINDSTR_FLAGS
                    .iter()
                    .any(|flag| word.eq_ignore_ascii_case(flag));
        }
    }
    let wildcard = operand.contains(['*', '?']);
    if !pattern_seen
        || !(recursive || wildcard)
        || operand.contains('/')
        || operand.ends_with('\\')
        || !operand.is_ascii()
        || if wildcard {
            !(path::local_dir_path(operand) && path::list_path(operand, Shell::Cmd))
        } else {
            !findstr_candidate(operand)
        }
        || !allowed_findstr(&args)
        || !stock::allows(command, &[command], Shell::Cmd)
    {
        return None;
    }
    Some(FindstrSearch {
        args,
        operand,
        recursive,
    })
}

#[cfg(windows)]
pub(super) fn findstr_candidate(path: &str) -> bool {
    !path
        .split('\\')
        .any(|part| part.starts_with('.') && part != ".")
        && local_read_path(path, Shell::Cmd, false)
        && read_path(path, Shell::Cmd)
}

fn native_shell() -> Shell {
    if cfg!(windows) {
        Shell::Cmd
    } else {
        Shell::Posix
    }
}

fn allows_for(command: &str, shell: Shell, context: Option<&Context<'_>>) -> bool {
    if !valid_input(command, shell) {
        return false;
    }

    // We launch CMD with /v:off. A single bang in a literal or executable path
    // has no closing delimiter. Pipelines are reparsed by another CMD.
    let cmd_bang = shell == Shell::Cmd && command.contains('!');
    if cmd_bang && command.matches('!').count() > 1 {
        return false;
    }

    if shell == Shell::Cmd {
        let mut words = command.split_ascii_whitespace();
        let program = words.next();
        if program
            .is_some_and(|name| name.eq_ignore_ascii_case("if") || name.eq_ignore_ascii_case("for"))
        {
            return cmd_control::allows(command, context);
        }
        if program.is_some_and(|name| name.eq_ignore_ascii_case("rem")) {
            return words.all(|word| {
                word.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            }) && stock::allows(command, &[command], shell);
        }
    }

    let mut source = Source {
        rest: command,
        shell,
        literal_git: false,
    };
    let mut words = Vec::new();
    let mut stages = Vec::new();
    let mut stage_start = command;
    let mut can_end = false;
    let mut has_pipe = false;
    let mut cmd_pipe_reparse_risk = cmd_bang;
    loop {
        let before = source.rest;
        match source.next() {
            Ok(Some(Token::Word(word, quote))) => {
                let quoted_program = words.is_empty() && quote.is_some();
                if quoted_program
                    && (trusted_external(word, shell, context).is_none()
                        || (shell == Shell::Cmd && !absolute_cmd_path(word)))
                {
                    return false;
                }
                cmd_pipe_reparse_risk |= shell == Shell::Cmd
                    && quote.is_some()
                    && (quoted_program || word.contains(['<', '>', '|', '&', '(', ')']));
                words.push((word, quote));
            }
            Ok(Some(Token::FileRedirect { output })) if !words.is_empty() => {
                let Ok(Some(Token::Word(path, _))) = source.next() else {
                    return false;
                };
                let valid = if output {
                    session_operand(path, context).is_some_and(|local| {
                        (shell == Shell::Cmd && path.eq_ignore_ascii_case("nul"))
                            || relative_path(local, shell, false)
                    })
                } else {
                    read_path(path, shell)
                };
                if !valid {
                    return false;
                }
            }
            Ok(Some(Token::DescriptorRedirect)) if !words.is_empty() => {}
            Ok(Some(Token::Newline)) if words.is_empty() => {
                stage_start = source.rest;
            }
            Ok(Some(token @ (Token::Newline | Token::End | Token::Join | Token::Pipe)))
                if !words.is_empty() =>
            {
                if !allowed_command(&words, shell, context) {
                    return false;
                }
                words.clear();
                has_pipe |= matches!(token, Token::Pipe);
                can_end = matches!(token, Token::Newline | Token::End);
                stages.push(&stage_start[..stage_start.len() - before.len()]);
                stage_start = source.rest;
            }
            Ok(None) => {
                if words.is_empty() {
                    if !can_end {
                        return false;
                    }
                } else {
                    if !allowed_command(&words, shell, context) {
                        return false;
                    }
                    stages.push(stage_start);
                }
                return !(has_pipe && cmd_pipe_reparse_risk)
                    && stock::allows(command, &stages, shell);
            }
            _ => return false,
        }
    }
}

fn valid_input(command: &str, shell: Shell) -> bool {
    !(command.len() > MAX_COMMAND_LENGTH
        || command.chars().any(|ch| {
            (ch.is_control() && !matches!(ch, '\t' | '\n'))
                || matches!(ch, '\u{00ad}' | '\u{061c}' | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}'
                | '\u{fe00}'..='\u{fe0f}' | '\u{feff}' | '\u{e0000}'..='\u{e01ef}')
                || (shell == Shell::Cmd && matches!(ch, '\n' | '^'))
        }))
}

pub(super) fn git_query<'a>(
    command: &'a str,
    context: &Context<'_>,
) -> Option<(std::path::PathBuf, Vec<&'a str>)> {
    let shell = native_shell();
    if !valid_input(command, shell) {
        return None;
    }
    let mut source = Source {
        rest: command,
        shell,
        literal_git: true,
    };
    let Some(Token::Word(name, quote)) = source.next().ok()? else {
        return None;
    };
    if expansion_present(name, quote, shell)
        || !matches!(
            trusted_external(name, shell, Some(context)),
            Some(External::Git)
        )
    {
        return None;
    }
    let mut args = Vec::new();
    while let Some(token) = source.next().ok()? {
        let Token::Word(word, quote) = token else {
            return None;
        };
        if expansion_present(word, quote, shell) {
            return None;
        }
        args.push(word);
    }
    if !git::allows(&args, shell)
        || stock::program_options()?.executes_program("git", &args)
        || !stock::allows(command, &[command], shell)
    {
        return None;
    }
    let executable = context.trusted_programs.iter().find(|program| {
        Path::new(program).is_absolute()
            && match shell {
                Shell::Posix => *program == &name || (name == "git" && program.ends_with("/git")),
                Shell::Cmd => {
                    program.eq_ignore_ascii_case(name)
                        || (matches!(name.to_ascii_lowercase().as_str(), "git" | "git.exe")
                            && program_basename(program, shell).eq_ignore_ascii_case("git.exe"))
                }
            }
    })?;
    let executable = Path::new(executable).canonicalize().ok()?;
    if executable.starts_with(context.cwd) {
        return None;
    }
    Some((executable, args))
}

enum Token<'a> {
    Word(&'a str, Option<char>),
    FileRedirect { output: bool },
    DescriptorRedirect,
    Newline,
    End,
    Join,
    Pipe,
}

struct Source<'a> {
    rest: &'a str,
    shell: Shell,
    literal_git: bool,
}

impl<'a> Source<'a> {
    fn next(&mut self) -> Result<Option<Token<'a>>, ()> {
        let input = self.rest.trim_start_matches([' ', '\t']);
        if input.is_empty() {
            return Ok(None);
        }

        for operator in [">&1", ">&2", "1>&1", "1>&2", "2>&1", "2>&2"] {
            if let Some(rest) = input.strip_prefix(operator) {
                return self.take(rest, Token::DescriptorRedirect);
            }
        }
        for operator in ["1>>", "2>>", ">>", "1>", "2>", ">", "<"] {
            if let Some(rest) = input.strip_prefix(operator) {
                self.rest = rest;
                return Ok(Some(Token::FileRedirect {
                    output: operator != "<",
                }));
            }
        }
        for operator in ["&&", "||", "|", "&", ";", "\n"] {
            if let Some(rest) = input.strip_prefix(operator) {
                self.rest = rest;
                return match (operator, self.shell) {
                    ("&&" | "||", _) | ("&", Shell::Cmd) => Ok(Some(Token::Join)),
                    ("|", _) => Ok(Some(Token::Pipe)),
                    (";", Shell::Posix) => Ok(Some(Token::End)),
                    ("\n", Shell::Posix) => Ok(Some(Token::Newline)),
                    _ => Err(()),
                };
            }
        }

        let findstr_literal = self.shell == Shell::Cmd
            && input
                .get(..3)
                .is_some_and(|head| head.eq_ignore_ascii_case("/c:"))
            && input[3..].starts_with('"');
        let prefix = if findstr_literal { 3 } else { 0 };
        if let Some(quote) = input[prefix..]
            .chars()
            .next()
            .filter(|&ch| ch == '"' || (ch == '\'' && self.shell == Shell::Posix))
        {
            let after_opening = &input[prefix + 1..];
            let end = after_opening.find(quote).ok_or(())?;
            let literal = &after_opening[..end];
            if self.shell == Shell::Posix
                && quote == '"'
                && (literal.contains('\x60')
                    || literal.ends_with('\\')
                    || literal
                        .as_bytes()
                        .windows(2)
                        .any(|pair| pair[0] == b'\\' && matches!(pair[1], b'\\' | b'$' | b'\n')))
            {
                return Err(());
            }
            let word = if findstr_literal {
                &input[..prefix + end + 2]
            } else {
                literal
            };
            return self.take(&after_opening[end + 1..], Token::Word(word, Some(quote)));
        }

        let end = input
            .char_indices()
            .find(|&(_, ch)| {
                !(ch.is_alphanumeric()
                    || "_./:@+=,-%".contains(ch)
                    || (self.literal_git && ch == '~')
                    || (self.shell == Shell::Cmd && matches!(ch, '\\' | '!' | '*' | '?')))
            })
            .map_or(input.len(), |(index, _)| index);
        if end == 0 {
            return Err(());
        }
        self.take(&input[end..], Token::Word(&input[..end], None))
    }

    fn take(&mut self, rest: &'a str, token: Token<'a>) -> Result<Option<Token<'a>>, ()> {
        if !rest.is_empty() && !rest.starts_with([' ', '\t', '\n', '<', '>', '|', '&', ';']) {
            return Err(());
        }
        self.rest = rest;
        Ok(Some(token))
    }
}

fn allowed_command(
    words: &[(&str, Option<char>)],
    shell: Shell,
    context: Option<&Context<'_>>,
) -> bool {
    let [(name, name_quote), args @ ..] = words else {
        return false;
    };
    if expansion_present(name, *name_quote, shell) || name.contains(['*', '?']) {
        return false;
    }
    for (index, &(argument, quote)) in args.iter().enumerate() {
        if shell == Shell::Cmd
            && argument.contains(['*', '?'])
            && !name.eq_ignore_ascii_case("dir")
            && quote.is_none()
        {
            return false;
        }
        if expansion_present(argument, quote, shell) {
            let data_argument = match shell {
                Shell::Posix => (*name == "echo" && index > 0) || (*name == "printf" && index > 0),
                Shell::Cmd => name.eq_ignore_ascii_case("echo"),
            };
            // echo can treat its first POSIX argument as an option. Require a known
            // data-shaped value there, and keep printf's format a literal.
            let echo_first = shell == Shell::Posix && *name == "echo" && index == 0;
            if !(data_argument || echo_first)
                || !safe_expansions(argument, shell, context, echo_first)
            {
                return false;
            }
        }
    }

    let first_arg_quoted = args.first().is_some_and(|(_, quote)| quote.is_some());
    let args: Vec<_> = args.iter().map(|&(word, _)| word).collect();
    let Some(options) = stock::program_options() else {
        return false;
    };
    let program = program_basename(name, shell).to_ascii_lowercase();
    let tool = program.strip_suffix(".exe").unwrap_or(&program);
    if options.executes_program(&program, &args)
        || (tool != program && options.executes_program(tool, &args))
    {
        return false;
    }
    if context.is_some() && queries::allows(name, &args, shell) {
        return true;
    }
    match trusted_external(name, shell, context) {
        Some(External::Python) => python_query(&args) || local_python_script(&args, shell, context),
        Some(External::Git) => matches!(
            args.as_slice(),
            ["version" | "--version"] | ["version" | "--version", "--build-options"]
        ),
        Some(External::Findstr) => allowed_findstr(&args),
        Some(External::Find) => matches!(args.as_slice(), [pattern, path]
            if first_arg_quoted && !pattern.is_empty()
                && relative_path(path, Shell::Cmd, false)),
        Some(External::Fc) => matches!(args.as_slice(), [first, second]
            if relative_path(first, Shell::Cmd, false)
                && relative_path(second, Shell::Cmd, false)),
        Some(External::Timeout) => matches!(args.as_slice(), [flag, seconds]
            if flag.eq_ignore_ascii_case("/t")
                && seconds.bytes().all(|byte| byte.is_ascii_digit())
                && seconds.parse::<u8>().is_ok_and(|seconds| seconds <= 60)),
        Some(External::Ripgrep) => context
            .zip(options.read_tool("rg"))
            .is_some_and(|(context, options)| allowed_ripgrep(&args, context, options, shell)),
        None => match shell {
            Shell::Posix => allowed_posix(name, &args),
            Shell::Cmd => allowed_cmd(&name.to_ascii_lowercase(), &args),
        },
    }
}

enum External {
    Python,
    Git,
    Findstr,
    Find,
    Fc,
    Timeout,
    Ripgrep,
}

fn trusted_external(name: &str, shell: Shell, context: Option<&Context<'_>>) -> Option<External> {
    let context = context?;
    if !context.trusted_programs.iter().any(|trusted| match shell {
        Shell::Posix => name == *trusted,
        Shell::Cmd => name.eq_ignore_ascii_case(trusted),
    }) {
        return None;
    }
    let base = program_basename(name, shell);
    let name = if shell == Shell::Cmd {
        base.to_ascii_lowercase()
    } else {
        base.to_owned()
    };
    let name = if shell == Shell::Cmd {
        name.strip_suffix(".exe").unwrap_or(&name)
    } else {
        name.as_str()
    };
    match name {
        "git" => Some(External::Git),
        "rg" => Some(External::Ripgrep),
        "findstr" if shell == Shell::Cmd => Some(External::Findstr),
        "find" if shell == Shell::Cmd => Some(External::Find),
        "fc" if shell == Shell::Cmd => Some(External::Fc),
        "timeout" if shell == Shell::Cmd => Some(External::Timeout),
        "python" | "python3" => Some(External::Python),
        _ if name.strip_prefix("python3.").is_some_and(|version| {
            !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
        }) =>
        {
            Some(External::Python)
        }
        _ => None,
    }
}

fn program_basename(name: &str, shell: Shell) -> &str {
    name.rsplit(|ch| ch == '/' || (shell == Shell::Cmd && ch == '\\'))
        .next()
        .unwrap_or(name)
}

fn absolute_cmd_path(word: &str) -> bool {
    matches!(word.as_bytes(), [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic())
        || word.starts_with("\\\\")
}

fn python_query(args: &[&str]) -> bool {
    matches!(args, ["-h" | "--help" | "-V" | "-VV" | "--version"])
}

fn local_python_script(args: &[&str], shell: Shell, context: Option<&Context<'_>>) -> bool {
    let first_operand = args
        .iter()
        .position(|arg| {
            !matches!(
                *arg,
                "-B" | "-E" | "-I" | "-O" | "-OO" | "-q" | "-s" | "-S" | "-u"
            )
        })
        .unwrap_or(args.len());
    let args = &args[first_operand..];
    let args = args.strip_prefix(&["--"]).unwrap_or(args);
    let Some(script) = args.first() else {
        return false;
    };
    let Some(script) = session_operand(script, context) else {
        return false;
    };
    let syntax = match shell {
        Shell::Posix => script.to_owned(),
        Shell::Cmd => script.replace('/', "\\"),
    };
    let extension = Path::new(script)
        .extension()
        .and_then(|extension| extension.to_str());
    relative_path(&syntax, shell, false)
        && extension.is_some_and(|extension| extension.eq_ignore_ascii_case("py"))
}

fn allowed_ripgrep(
    args: &[&str],
    context: &Context<'_>,
    options: &options::ReadTool,
    shell: Shell,
) -> bool {
    let mut args = args.iter().copied();
    let mut operands = Vec::new();
    let mut explicit_pattern = false;
    let mut files_only = false;
    let mut no_config = false;
    while let Some(token) = args.next() {
        if token == "--" {
            operands.extend(args);
            break;
        }
        if token.starts_with("--") {
            let (option, attached) = options::split_option(token);
            if matches!(option, "--pre" | "--hostname-bin") {
                return false;
            }
            if options.long_takes_argument(option) {
                let Some(value) = attached.or_else(|| args.next()) else {
                    return false;
                };
                if matches!(option, "--file" | "--ignore-file")
                    && value != "-"
                    && !local_read_path(value, shell, false)
                {
                    return false;
                }
                explicit_pattern |= matches!(option, "--regexp" | "--file");
            } else if attached.is_none() {
                match option {
                    "--files" | "--help" | "--version" => files_only = true,
                    "--no-config" => no_config = true,
                    "--byte-offset"
                    | "--column"
                    | "--count"
                    | "--count-matches"
                    | "--files-with-matches"
                    | "--files-without-match"
                    | "--fixed-strings"
                    | "--glob-case-insensitive"
                    | "--heading"
                    | "--ignore-case"
                    | "--invert-match"
                    | "--json"
                    | "--line-buffered"
                    | "--line-number"
                    | "--max-columns-preview"
                    | "--multiline"
                    | "--multiline-dotall"
                    | "--no-filename"
                    | "--no-heading"
                    | "--no-ignore"
                    | "--no-line-number"
                    | "--only-matching"
                    | "--passthru"
                    | "--quiet"
                    | "--smart-case"
                    | "--stats"
                    | "--text"
                    | "--with-filename"
                    | "--word-regexp" => {}
                    _ => return false,
                }
            } else {
                return false;
            }
        } else if let Some(flags) = token.strip_prefix('-').filter(|flags| !flags.is_empty()) {
            for (index, flag) in flags.char_indices() {
                if options.short_takes_argument(flag) {
                    let attached = &flags[index + flag.len_utf8()..];
                    let value = if attached.is_empty() {
                        args.next()
                    } else {
                        Some(attached)
                    };
                    let Some(value) = value else {
                        return false;
                    };
                    if flag == 'f' && value != "-" && !local_read_path(value, shell, false) {
                        return false;
                    }
                    explicit_pattern |= matches!(flag, 'e' | 'f');
                    break;
                }
                if matches!(flag, 'h' | 'V') {
                    files_only = true;
                } else if !"abciIlLFnoPqSsUvwx".contains(flag) {
                    return false;
                }
            }
        } else {
            operands.push(token);
        }
    }

    let config_set = context.environment.iter().any(|(key, value)| {
        !value.is_empty()
            && key.to_str().is_some_and(|key| match shell {
                Shell::Posix => key == "RIPGREP_CONFIG_PATH",
                Shell::Cmd => key.eq_ignore_ascii_case("RIPGREP_CONFIG_PATH"),
            })
    });
    if config_set && !no_config {
        return false;
    }
    let paths = if explicit_pattern || files_only {
        operands.as_slice()
    } else {
        let [_, paths @ ..] = operands.as_slice() else {
            return false;
        };
        paths
    };
    paths
        .iter()
        .all(|path| *path == "-" || local_read_path(path, shell, true))
}

const FINDSTR_FLAGS: [&str; 7] = ["/i", "/l", "/m", "/n", "/r", "/v", "/x"];

fn allowed_findstr(args: &[&str]) -> bool {
    let mut args = args.iter().copied().peekable();
    while args.peek().is_some_and(|arg| {
        FINDSTR_FLAGS
            .iter()
            .any(|flag| arg.eq_ignore_ascii_case(flag))
    }) {
        args.next();
    }
    let Some(pattern) = args.next() else {
        return false;
    };
    let pattern = if pattern
        .get(..3)
        .is_some_and(|arg| arg.eq_ignore_ascii_case("/c:"))
    {
        let value = &pattern[3..];
        value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value)
    } else if pattern.starts_with('/') {
        return false;
    } else {
        pattern
    };
    !pattern.is_empty()
        && !pattern.ends_with('\\')
        && args.all(|path| local_read_path(path, Shell::Cmd, false))
}

fn expansion_present(word: &str, quote: Option<char>, shell: Shell) -> bool {
    match shell {
        Shell::Posix => quote == Some('"') && word.contains('$'),
        Shell::Cmd => word.contains('%'),
    }
}

fn safe_expansions(
    mut text: &str,
    shell: Shell,
    context: Option<&Context<'_>>,
    echo_first: bool,
) -> bool {
    let Some(context) = context else {
        return false;
    };
    let marker = match shell {
        Shell::Posix => '$',
        Shell::Cmd => '%',
    };
    while let Some((_, after_marker)) = text.split_once(marker) {
        let (name, rest) = match shell {
            Shell::Cmd => match after_marker.split_once('%') {
                Some(parts) => parts,
                None => return false,
            },
            Shell::Posix if after_marker.starts_with('{') => {
                match after_marker[1..].split_once('}') {
                    Some(parts) => parts,
                    None => return false,
                }
            }
            Shell::Posix => {
                let name_end = after_marker
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .count();
                after_marker.split_at(name_end)
            }
        };
        if !name.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_')
            || !name
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
            || credential_variable(name)
        {
            return false;
        }
        let value = context.environment.iter().find_map(|(key, value)| {
            let key = key.to_str()?;
            let matches = match shell {
                Shell::Posix => key == name,
                Shell::Cmd => key.eq_ignore_ascii_case(name),
            };
            matches.then(|| value.to_str()).flatten()
        });
        let value = value.or_else(|| {
            if shell == Shell::Posix && name == "PWD" {
                context.cwd.to_str()
            } else {
                None
            }
        });
        let Some(value) = value else {
            return false;
        };
        if (shell == Shell::Cmd
            && !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b" ._-/\\:@+=,".contains(&b)))
            || (echo_first && value.starts_with('-'))
        {
            return false;
        }
        text = rest;
    }
    true
}

fn credential_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.split('_').any(|part| {
        matches!(
            part,
            "KEY"
                | "APIKEY"
                | "TOKEN"
                | "SECRET"
                | "PASSWORD"
                | "PASSWD"
                | "CREDENTIAL"
                | "CREDENTIALS"
        )
    })
}

fn allowed_posix(name: &str, args: &[&str]) -> bool {
    match name {
        "echo" => true,
        "printf" => args.first().is_some_and(|format| printf_format(format)),
        "pwd" => matches!(args, [] | ["-L" | "-P"]),
        "true" | "false" => args.is_empty(),
        "exit" => matches!(args, [] | [_]) && args.first().is_none_or(|arg| exit_code(arg)),
        "sleep" => matches!(args, [duration] if sleep_duration(duration)),
        "cd" => matches!(args, [path] if relative_path(path, Shell::Posix, true)),
        "ls" => short_options(args, "ahl1")
            .iter()
            .all(|path| path::list_path(path, Shell::Posix)),
        "cat" => file_operands(short_options(args, "n")),
        "wc" => file_operands(short_options(args, "clmw")),
        "sort" => file_operands(short_options(args, "finru")),
        "uniq" => {
            let paths = short_options(args, "cdiu");
            paths.len() <= 1 && file_operands(paths)
        }
        "grep" => {
            let (args, end_of_options) = options(args, "Fcinvwx");
            matches!(args, [pattern, paths @ ..]
                if (end_of_options || !pattern.starts_with('-')) && file_operands(paths))
        }
        "head" | "tail" => match args {
            ["-n" | "-c", count, paths @ ..] if count.bytes().all(|b| b.is_ascii_digit()) => {
                count.parse::<u16>().is_ok() && file_operands(paths)
            }
            paths => file_operands(paths),
        },
        "mkdir" | "touch" => match args {
            ["-p", paths @ ..] if name == "mkdir" => !paths.is_empty() && write_operands(paths),
            paths => !paths.is_empty() && write_operands(paths),
        },
        _ => false,
    }
}

fn allowed_cmd(name: &str, args: &[&str]) -> bool {
    match name {
        "echo" => true,
        "ver" => args.is_empty(),
        "cd" => {
            matches!(args, []) || matches!(args, [path] if relative_path(path, Shell::Cmd, true))
        }
        "dir" => args
            .iter()
            .all(|arg| path::dir_flag(arg) || path::list_path(arg, Shell::Cmd)),
        "type" => !args.is_empty() && args.iter().all(|arg| read_path(arg, Shell::Cmd)),
        "mkdir" | "md" => {
            !args.is_empty() && args.iter().all(|arg| relative_path(arg, Shell::Cmd, false))
        }
        "exit" => match args {
            [] => true,
            [arg] => arg.eq_ignore_ascii_case("/b") || exit_code(arg),
            [flag, code] => flag.eq_ignore_ascii_case("/b") && exit_code(code),
            _ => false,
        },
        _ => false,
    }
}

// Each listed short option is a flag without an option operand.
fn options<'a, 'word>(args: &'a [&'word str], flags: &str) -> (&'a [&'word str], bool) {
    let first_operand = args
        .iter()
        .position(|arg| {
            !arg.strip_prefix('-').is_some_and(|chars| {
                !chars.is_empty() && chars.chars().all(|ch| flags.contains(ch))
            })
        })
        .unwrap_or(args.len());
    match &args[first_operand..] {
        ["--", rest @ ..] => (rest, true),
        rest => (rest, false),
    }
}

fn short_options<'a, 'word>(args: &'a [&'word str], flags: &str) -> &'a [&'word str] {
    options(args, flags).0
}

fn file_operands(args: &[&str]) -> bool {
    args.iter().all(|path| read_path(path, Shell::Posix))
}

fn write_operands(args: &[&str]) -> bool {
    args.iter()
        .all(|path| relative_path(path, Shell::Posix, false))
}

/// Validate an absolute spelling using the same rules as its relative spelling
/// under the session's launch directory. The shell still receives the original.
fn session_operand<'a>(word: &'a str, context: Option<&Context<'_>>) -> Option<&'a str> {
    let path = Path::new(word);
    if !path.is_absolute() {
        return Some(word);
    }
    let cwd = context?.cwd;
    #[cfg(windows)]
    let cwd = {
        // Windows canonicalize() returns a verbatim path. Accept only ordinary
        // drive-qualified input when comparing with the session directory.
        if !matches!(word.as_bytes(), [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic())
        {
            return None;
        }
        let root = cwd.to_str()?;
        Path::new(root.strip_prefix(r"\\?\").unwrap_or(root))
    };
    path.strip_prefix(cwd).ok()?.to_str()
}

fn relative_path(path: &str, shell: Shell, allow_cwd: bool) -> bool {
    let separator = match shell {
        Shell::Posix => '/',
        Shell::Cmd => '\\',
    };
    if allow_cwd
        && (path == "."
            || path
                .strip_prefix('.')
                .is_some_and(|rest| rest.len() == 1 && rest.starts_with(separator)))
    {
        return true;
    }
    let path = path
        .strip_prefix('.')
        .and_then(|path| path.strip_prefix(separator))
        .unwrap_or(path);
    let path = if allow_cwd {
        path.strip_suffix(separator).unwrap_or(path)
    } else {
        path
    };
    path.split(|ch| ch == separator || (shell == Shell::Cmd && ch == '/'))
        .all(|component| {
            component.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
                && component
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b))
                && !component.eq_ignore_ascii_case("config.yaml")
                && (shell != Shell::Cmd || windows_path_component(component))
        })
}

fn windows_path_component(component: &str) -> bool {
    if component.ends_with([' ', '.']) {
        return false;
    }
    let base = component.split('.').next().unwrap_or_default().trim_end();
    let base = base.to_ascii_uppercase();
    !matches!(base.as_str(), "AUX" | "CON" | "NUL" | "PRN")
        && !base
            .strip_prefix("COM")
            .or_else(|| base.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
}

fn printf_format(format: &str) -> bool {
    if format.starts_with('-') {
        return false;
    }
    let mut chars = format.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' && !matches!(chars.next(), Some('s' | '%')) {
            return false;
        }
    }
    true
}

fn exit_code(arg: &str) -> bool {
    arg.bytes().all(|b| b.is_ascii_digit()) && arg.parse::<u8>().is_ok()
}

fn sleep_duration(arg: &str) -> bool {
    arg.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && arg.parse::<f64>().is_ok_and(|seconds| seconds <= 60.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_read_paths_and_windows_listing_are_classified() {
        for (shell, command) in [
            (Shell::Posix, "cat .gitignore"),
            (Shell::Posix, "cat ../shared/notes.txt"),
            (Shell::Posix, "cat /tmp/notes.txt"),
            (Shell::Posix, "wc < ../shared/notes.txt"),
            (Shell::Posix, "ls -a ../.config"),
            (Shell::Cmd, "type ..\\shared\\notes.txt"),
            (Shell::Cmd, "type C:\\shared\\notes.txt"),
            (Shell::Cmd, "dir C:\\"),
            (Shell::Cmd, "dir ..\\.config"),
            (Shell::Cmd, "echo found < C:\\shared\\notes.txt"),
            (Shell::Posix, "cat résumé+notes.txt"),
            (Shell::Posix, "wc < .gitignore"),
            (Shell::Cmd, "type .gitignore"),
            (Shell::Cmd, "type folder\\résumé+notes.txt"),
            (Shell::Cmd, "type folder/résumé+notes.txt"),
            (Shell::Cmd, "dir /a:d /o:n folder\\*.txt"),
            (Shell::Cmd, "dir /A:-D /O:-N ."),
        ] {
            assert!(allows_for(command, shell, None), "{command}");
        }
        for (shell, command) in [
            (Shell::Posix, "echo no > .gitignore"),
            (Shell::Posix, "touch .gitignore"),
            (Shell::Posix, "cat config.yaml"),
            (Shell::Posix, "cat .env"),
            (Shell::Posix, "cat ../.ssh/id_rsa"),
            (Shell::Posix, "ls ../.aws"),
            (Shell::Posix, "wc < /tmp/config.yaml"),
            (Shell::Cmd, "type .env"),
            (Shell::Cmd, "type config.yaml"),
            (Shell::Cmd, "type C:\\shared\\credentials.json"),
            (Shell::Cmd, "type \\\\server\\share\\notes.txt"),
            (Shell::Cmd, "type \\\\.\\C:\\notes.txt"),
            (Shell::Cmd, "dir /s"),
            (Shell::Cmd, "dir ..\\.ssh"),
            (Shell::Cmd, "type folder\\*.txt"),
            (Shell::Cmd, "echo no > .gitignore"),
            (Shell::Cmd, "type ../private"),
            (Shell::Cmd, "type folder/s"),
            (Shell::Cmd, "type folder\\CON.txt"),
            (Shell::Cmd, "dir /a:unknown"),
            (Shell::Cmd, "dir /o:x"),
        ] {
            assert!(!allows_for(command, shell, None), "{command}");
        }
        let context = Context {
            cwd: Path::new("."),
            environment: &[],
            trusted_programs: &["rg", "findstr"],
        };
        assert!(allows_for(
            "rg --no-config --ignore-file .gitignore needle folder\\notes.txt",
            Shell::Cmd,
            Some(&context)
        ));
        for command in [
            "findstr /n one .gitignore",
            "findstr /l /c:\"one two\" folder\\notes.txt",
            "findstr /l /c:\"one two\" folder/notes.txt",
        ] {
            assert!(allows_for(command, Shell::Cmd, Some(&context)), "{command}");
        }
        for command in ["findstr /n one config.yaml", "findstr /n one *.txt"] {
            assert!(
                !allows_for(command, Shell::Cmd, Some(&context)),
                "{command}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn admitted_cmd_reads_and_lists_real_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        std::fs::write(
            dir.path().join("folder").join("résumé+notes.txt"),
            "fixture",
        )
        .unwrap();
        std::fs::write(dir.path().join("folder").join("notes.txt"), "one two\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignore-this\n").unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (command, expected) in [
            ("dir /a:d /o:n", "folder"),
            ("type .gitignore", "ignore-this"),
            ("dir /b folder\\*.txt", "notes.txt"),
            ("type folder\\résumé+notes.txt", "fixture"),
            ("findstr /l /c:\"one two\" folder\\notes.txt", "one two"),
        ] {
            let output = runtime
                .block_on(super::super::Command::parse(command).unwrap().capture(
                    &cwd,
                    tokio_util::sync::CancellationToken::new(),
                    &[],
                ))
                .unwrap();
            assert_eq!(output.code, 0, "{command}: {}", output.stderr);
            assert!(
                output.stdout.contains(expected),
                "{command}: {}",
                output.stdout
            );
        }
        use std::os::windows::process::CommandExt;
        let cmd = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        for command in [
            "type folder/résumé+notes.txt",
            "findstr /l /c:\"one two\" folder/notes.txt",
        ] {
            let direct = std::process::Command::new(&cmd)
                .current_dir(&cwd)
                .args(["/d", "/v:off", "/s", "/c"])
                .raw_arg(format!("\"{command}\""))
                .output()
                .unwrap();
            let captured = runtime
                .block_on(super::super::Command::parse(command).unwrap().capture(
                    &cwd,
                    tokio_util::sync::CancellationToken::new(),
                    &[],
                ))
                .unwrap();
            assert_eq!(captured.code, direct.status.code().unwrap(), "{command}");
            assert_eq!(
                captured.stderr.trim(),
                String::from_utf8_lossy(&direct.stderr).trim(),
                "{command}",
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn reads_from_the_chat_starting_directory() {
        let root = tempfile::Builder::new()
            .prefix("composer-")
            .tempdir()
            .unwrap();
        let chat = root.path().join("chat");
        let sibling = root.path().join("sibling");
        std::fs::create_dir(&chat).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        let config = root.path().join(".config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(config.join("notes.txt"), "listing fixture").unwrap();
        let notes = sibling.join("notes.txt");
        let missing = sibling.join("missing.txt");
        std::fs::write(&notes, "sibling fixture").unwrap();
        let cwd = chat.canonicalize().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (command, expected) in [
            (
                r"type ..\sibling\notes.txt".to_owned(),
                Some("sibling fixture"),
            ),
            (
                format!("type \"{}\"", notes.display()),
                Some("sibling fixture"),
            ),
            (r"dir /b ..\sibling".to_owned(), Some("notes.txt")),
            (
                r"echo redirected < ..\sibling\notes.txt".to_owned(),
                Some("redirected"),
            ),
            (r"echo redirected < ..\sibling\missing.txt".to_owned(), None),
            (r"dir /b ..\.config".to_owned(), Some("notes.txt")),
            (r"type ..\sibling\missing.txt".to_owned(), None),
            (format!("type \"{}\"", missing.display()), None),
        ] {
            let output = runtime
                .block_on(super::super::Command::parse(&command).unwrap().capture(
                    &cwd,
                    tokio_util::sync::CancellationToken::new(),
                    &[],
                ))
                .unwrap();
            if let Some(expected) = expected {
                assert_eq!(output.code, 0, "{command}: {}", output.stderr);
                assert!(
                    output.stdout.contains(expected),
                    "{command}: {}",
                    output.stdout
                );
            } else {
                assert_ne!(output.code, 0, "{command}");
                assert!(!output.stderr.is_empty(), "{command}");
            }
        }
    }

    #[test]
    fn posix_allows_literal_data_and_local_file_operations() {
        for command in [
            "pwd; ls -la; exit 3",
            "printf '%s\\n' 'use | literally' >&2",
            "printf '%s\\n' alpha beta | grep -F alpha > 'out file.txt' && cat 'out file.txt'",
            "mkdir -p reports && touch reports/summary.txt; wc -l < reports/summary.txt",
            "echo 'a $literal and home ~' >> ./log.txt\nsort -u log.txt | head -n 1",
            "echo test |\n cat",
            "false || echo fallback",
        ] {
            assert!(allows_for(command, Shell::Posix, None), "{command}");
        }
    }

    #[test]
    fn absolute_operands_stay_within_the_session_directory() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let context = Context {
            cwd: &cwd,
            environment: &[],
            trusted_programs: &[],
        };
        let ordinary = root.path().join("output.txt");
        let sibling = root.path().with_extension("sibling").join("output.txt");
        assert_eq!(
            session_operand(ordinary.to_str().unwrap(), Some(&context)),
            Some("output.txt")
        );
        assert_eq!(
            session_operand(sibling.to_str().unwrap(), Some(&context)),
            None
        );
        let script = root.path().join("probe.py");
        assert!(local_python_script(
            &[script.to_str().unwrap()],
            native_shell(),
            Some(&context)
        ));
    }

    #[test]
    fn cmd_accepts_internal_commands_and_local_file_operations() {
        for command in [
            "CD && DIR /b",
            "echo café",
            "echo plain!text",
            "echo ok > nul",
            "echo ok 2>NUL",
            "echo ok > \"output folder/output file.txt\"",
            "echo hello > \"out file.txt\" & type \"out file.txt\"",
            "echo hello 2>&1 | echo one",
            "md reports && type reports\\summary.txt || exit /b 3",
        ] {
            assert!(allows_for(command, Shell::Cmd, None), "{command}");
        }
    }

    #[test]
    fn contextual_commands_need_a_trusted_program_and_classified_invocation() {
        let context = Context {
            cwd: Path::new("."),
            environment: &[],
            trusted_programs: &[
                "python",
                "python.exe",
                "findstr",
                "findstr.exe",
                "git",
                "git.exe",
                "rg",
                "rg.exe",
                r"C:\Program Files\Python\python.exe",
                "/opt/tools/python3.13",
            ],
        };
        for (shell, command) in [
            (Shell::Posix, "python probe.py 'two words'"),
            (Shell::Posix, "python ./probe.py | cat"),
            (Shell::Posix, "python missing.py"),
            (Shell::Posix, "python -B -u -- missing.py"),
            (Shell::Posix, "python --version"),
            (Shell::Posix, "cd notes && python ./probe.py"),
            (Shell::Posix, "rg -nF 'literal phrase' README.md"),
            (Shell::Posix, "rg --files crates"),
            (Shell::Cmd, "rg.exe -n \"literal phrase\" README.md"),
            (
                Shell::Cmd,
                "python.exe probe.py args \"two words\" \"left|right\" \"\"",
            ),
            (
                Shell::Cmd,
                "echo fixture-pipe | findstr /C:fixture-pipe > \"captured file.txt\"",
            ),
            (Shell::Cmd, "findstr.exe /i /C:\"two words\" report.txt"),
            (
                Shell::Cmd,
                "\"C:\\Program Files\\Python\\python.exe\" probe.py",
            ),
            (Shell::Posix, "/opt/tools/python3.13 probe.py"),
            (Shell::Posix, "git --version"),
            (Shell::Cmd, "git.exe version --build-options"),
        ] {
            assert!(allows_for(command, shell, Some(&context)), "{command}");
            assert!(!allows_for(command, shell, None), "{command}");
        }
        for (shell, command) in [
            (Shell::Posix, "python --help probe.py"),
            (Shell::Posix, "python -m example"),
            (Shell::Posix, "git status"),
            (Shell::Cmd, "no-such-program"),
            (Shell::Posix, "rg --pre processor needle"),
            (
                Shell::Cmd,
                "python.exe probe.py \"left|right\" | echo fixture",
            ),
            (Shell::Cmd, "findstr /F:inputs.txt fixture"),
            (Shell::Cmd, "findstr /C:fixture ../outside.txt"),
        ] {
            assert!(!allows_for(command, shell, Some(&context)), "{command}");
        }
    }

    #[test]
    fn windows_system_utilities_admit_literal_forms() {
        let context = Context {
            cwd: Path::new("."),
            environment: &[],
            trusted_programs: &["find", "fc", "timeout"],
        };
        for command in [
            r#"find "one" notes.txt"#,
            "fc notes.txt other.txt",
            "timeout /T 1",
        ] {
            assert!(allows_for(command, Shell::Cmd, Some(&context)), "{command}");
            assert!(!allows_for(command, Shell::Cmd, None), "{command}");
            assert!(
                !allows_for(command, Shell::Posix, Some(&context)),
                "{command}"
            );
        }
        for command in [
            "find one notes.txt",
            r"fc notes.txt ..\other.txt",
            "timeout /t 61",
            "timeout /t -1",
        ] {
            assert!(
                !allows_for(command, Shell::Cmd, Some(&context)),
                "{command}"
            );
        }
        assert!(allows_for("rem an ordinary comment", Shell::Cmd, None));
        assert!(!allows_for("rem a comment & echo next", Shell::Cmd, None));
        assert!(!allows_for("rem a comment > result.txt", Shell::Cmd, None));
    }

    #[cfg(windows)]
    #[test]
    fn windows_system_utilities_resolve_in_production_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for command in [
            r#"find "one" notes.txt"#,
            "more notes.txt",
            "fc notes.txt other.txt",
            "timeout /t 1",
            "rem an ordinary comment",
        ] {
            crate::shell::Command::parse(command)
                .unwrap()
                .check(&cwd)
                .unwrap_or_else(|error| panic!("{command}: {error:?}"));
        }
        for command in [
            "more config.yaml",
            r"more ..\notes.txt",
            "more notes.txt & echo next",
            "more notes.txt > output.txt",
        ] {
            assert!(matches!(
                crate::shell::Command::parse(command).unwrap().check(&cwd),
                Err(crate::shell::Error::Unsupported)
            ));
        }
    }

    #[test]
    fn ripgrep_configuration_follows_the_frozen_environment() {
        let context = Context {
            cwd: Path::new("."),
            environment: &[(
                OsString::from("RIPGREP_CONFIG_PATH"),
                OsString::from("config"),
            )],
            trusted_programs: &["rg"],
        };
        assert!(!allows_for(
            "rg needle README.md",
            Shell::Posix,
            Some(&context)
        ));
        assert!(allows_for(
            "rg --no-config needle README.md",
            Shell::Posix,
            Some(&context)
        ));
    }

    #[test]
    fn expands_frozen_values_only_in_literal_output_arguments() {
        let cwd = Path::new(".");
        let environment = [
            (
                OsString::from("COMPOSER_FIXTURE_ENV"),
                OsString::from("ordinary fixture value"),
            ),
            (
                OsString::from("SPECIAL_TEXT"),
                OsString::from("literal & text"),
            ),
            (
                OsString::from("EXAMPLE_TOKEN"),
                OsString::from("fixture value"),
            ),
        ];
        let context = Context {
            cwd,
            environment: &environment,
            trusted_programs: &[],
        };
        for (shell, command) in [
            (Shell::Posix, "printf \"%s\\n\" \"$COMPOSER_FIXTURE_ENV\""),
            (Shell::Posix, "echo \"text: ${COMPOSER_FIXTURE_ENV}\""),
            (Shell::Cmd, "echo %COMPOSER_FIXTURE_ENV%"),
        ] {
            assert!(allows_for(command, shell, Some(&context)), "{command}");
            assert!(!allows_for(command, shell, None), "{command}");
        }
        for (shell, command) in [
            (Shell::Posix, "printf \"$COMPOSER_FIXTURE_ENV\""),
            (Shell::Posix, "cat \"$COMPOSER_FIXTURE_ENV\""),
            (Shell::Posix, "echo \"$EXAMPLE_TOKEN\""),
            (Shell::Cmd, "echo %SPECIAL_TEXT%"),
            (Shell::Cmd, "type %COMPOSER_FIXTURE_ENV%"),
            (Shell::Cmd, "echo %MISSING_VALUE%"),
        ] {
            assert!(!allows_for(command, shell, Some(&context)), "{command}");
        }
    }

    #[test]
    fn rejects_unclassified_shell_syntax_without_executing_it() {
        for command in [
            "",
            "echo test |",
            "echo test &&",
            "echo hi > ../outside.txt",
            "echo hi > .env",
            "echo hi > config.yaml",
            "echo hi >| report.txt",
            "echo \u{202e}test",
            "exec echo hello",
            "echo hi > log.txt || unknown-command",
        ] {
            assert!(!allows_for(command, Shell::Posix, None), "{command}");
            assert!(!allows_for(command, Shell::Cmd, None), "{command}");
        }

        for command in [
            "echo hi & echo bye",
            "echo $(pwd)",
            "echo \"$PWD\"",
            "echo hello\\ world",
            "echo \"one\"\"two\"",
            "echo hi > /tmp/report.txt",
            "echo hi > ~/report.txt",
            "cat reports/*",
            "sort --compress-program=other",
            "grep -f patterns.txt report.txt",
            "head -n 3 /dev/zero",
            "printf '%10000s' test",
            "cat <<EOF",
        ] {
            assert!(!allows_for(command, Shell::Posix, None), "{command}");
        }

        for command in [
            "\"echo\" hello",
            "echo one\n echo two",
            "echo %CD%",
            "echo !CD!",
            "echo ^& echo two",
            "echo \"test & test\" | echo ok",
            "echo hi > C:\\report.txt",
            "echo hi > report.txt:stream",
            "type con.txt",
            "type nul",
            "echo data < nul",
            "echo data > \"out/../file.txt\"",
            "type prn .txt",
            "type \"COM1 .txt\"",
            "type \"file.txt \"",
            "findstr hello log.txt",
        ] {
            assert!(!allows_for(command, Shell::Cmd, None), "{command}");
        }
    }
}
