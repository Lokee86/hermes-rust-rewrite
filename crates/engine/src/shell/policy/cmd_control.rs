//! Literal, standalone CMD control statements.
use super::{
    Context, Shell, Source, Token, allowed_command,
    path::{self, local_read_path},
    stock,
};

pub(super) fn allows(command: &str, context: Option<&Context<'_>>) -> bool {
    let command = command.trim_matches([' ', '\t']);
    if command.contains(['!', '<', '>', '|', '&']) {
        return false;
    }
    let Some((name, rest)) = command.split_once([' ', '\t']) else {
        return false;
    };
    if name.eq_ignore_ascii_case("if") {
        if command.contains(['%', '(', ')', '*', '?']) {
            return false;
        }
        let Some(body) = if_exists(rest) else {
            return false;
        };
        stock::allows(command, &[body], Shell::Cmd)
    } else {
        for_echo(command)
            && !environment_expansion(command, context)
            && stock::allows(command, &[command], Shell::Cmd)
    }
}

// The FOR grammar admits two percent signs for the loop variable. Keep
// that pair independent of environment expansion in the native shell.
fn environment_expansion(command: &str, context: Option<&Context<'_>>) -> bool {
    let Some(context) = context else {
        return false;
    };
    let Some((name, _)) = command
        .split_once('%')
        .and_then(|(_, rest)| rest.split_once('%'))
    else {
        return false;
    };
    let name = name.to_uppercase();
    context
        .environment
        .iter()
        .any(|(key, _)| key.to_str().is_some_and(|key| key.to_uppercase() == name))
}

fn if_exists(rest: &str) -> Option<&str> {
    let mut source = Source {
        rest,
        shell: Shell::Cmd,
        literal_git: false,
    };
    let Token::Word(predicate, None) = source.next().ok()?? else {
        return None;
    };
    if !predicate.eq_ignore_ascii_case("exist") {
        return None;
    }
    let Token::Word(path, _) = source.next().ok()?? else {
        return None;
    };
    if !literal_path(path) {
        return None;
    }

    let body = source.rest.trim_start_matches([' ', '\t']);
    let mut words = Vec::new();
    while let Some(token) = source.next().ok()? {
        let Token::Word(word, quote) = token else {
            return None;
        };
        if word.eq_ignore_ascii_case("else") {
            return None;
        }
        words.push((word, quote));
    }
    let (name, None) = *words.first()? else {
        return None;
    };
    if !["dir", "echo", "type", "ver"]
        .iter()
        .any(|built_in| name.eq_ignore_ascii_case(built_in))
    {
        return None;
    }
    if name.eq_ignore_ascii_case("type") && !words[1..].iter().all(|(path, _)| literal_path(path)) {
        return None;
    }
    if name.eq_ignore_ascii_case("dir")
        && !words[1..]
            .iter()
            .all(|(arg, _)| path::dir_flag(arg) || path::local_dir_path(arg))
    {
        return None;
    }
    allowed_command(&words, Shell::Cmd, None).then_some(body)
}

fn for_echo(command: &str) -> bool {
    let Some((header, rest)) = command.split_once('(') else {
        return false;
    };
    let Some((files, body)) = rest.split_once(')') else {
        return false;
    };
    let mut header = header.split_ascii_whitespace();
    let (Some(program), Some(variable), Some(in_keyword), None) =
        (header.next(), header.next(), header.next(), header.next())
    else {
        return false;
    };
    if !program.eq_ignore_ascii_case("for")
        || !matches!(variable.as_bytes(), [b'%', name] if name.is_ascii_alphabetic())
        || !in_keyword.eq_ignore_ascii_case("in")
        || !body.starts_with([' ', '\t'])
    {
        return false;
    }
    let mut body = body.split_ascii_whitespace();
    if !matches!(
        (body.next(), body.next(), body.next(), body.next()),
        (Some(do_keyword), Some(echo), Some(value), None)
            if do_keyword.eq_ignore_ascii_case("do")
                && echo.eq_ignore_ascii_case("@echo")
                && value == variable
    ) {
        return false;
    }
    let mut files = files.split_ascii_whitespace().peekable();
    files.peek().is_some() && files.all(literal_path)
}

fn literal_path(path: &str) -> bool {
    !path.contains('/') && local_read_path(path, Shell::Cmd, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_control_forms() {
        for command in [
            "if exist notes.txt type notes.txt",
            "IF EXIST \"my notes.txt\" ECHO found",
            "for %f in (notes.txt) do @echo %f",
            "FOR %F IN (notes.txt other.txt) DO @ECHO %F",
        ] {
            assert!(allows(command, None), "{command}");
        }
        for command in [
            "if exist config.yaml type notes.txt",
            "if exist notes.txt mkdir notes",
            "if exist ..\\notes.txt type notes.txt",
            "if exist notes.txt type ..\\notes.txt",
            "if exist notes.txt dir C:\\notes",
            "for %f in (..\\notes.txt) do @echo %f",
            "if exist notes.txt if exist other.txt type notes.txt",
            "if exist notes.txt echo found & type notes.txt",
            "for %f in (*.txt) do @echo %f",
            "for %f in (notes.txt) do @echo %g",
            "for %f in (notes.txt) do @echo %f extra",
        ] {
            assert!(!allows(command, None), "{command}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn captures_literal_if_and_for() {
        use crate::shell::Command;
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        std::fs::write(cwd.join("notes.txt"), "fixture").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (command, expected) in [
            ("if exist notes.txt type notes.txt", "fixture"),
            ("if exist missing.txt type notes.txt", ""),
            ("for %f in (notes.txt) do @echo %f", "notes.txt"),
        ] {
            let output = runtime
                .block_on(Command::parse(command).unwrap().capture(
                    &cwd,
                    CancellationToken::new(),
                    &[],
                ))
                .unwrap();
            assert_eq!(output.code, 0, "{command}: {}", output.stderr);
            assert_eq!(output.stdout.trim(), expected, "{command}");
        }
    }
}
