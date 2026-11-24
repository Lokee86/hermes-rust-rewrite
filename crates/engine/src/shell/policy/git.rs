//! Standalone Git reads. Windows confines working-tree reads to a single process.
use super::{Shell, relative_path};

pub(in crate::shell) fn worktree_read(args: &[&str]) -> bool {
    matches!(args, ["status", "--short"] | ["diff", "--stat"])
}

pub(super) fn allows(args: &[&str], shell: Shell) -> bool {
    if cfg!(windows) && worktree_read(args) {
        return true;
    }
    let (log, mut args) = match args {
        ["log", rest @ ..] => (true, rest.iter().copied()),
        ["diff", "--cached" | "--staged", rest @ ..] => (false, rest.iter().copied()),
        _ => return false,
    };
    while let Some(word) = args.next() {
        match (log, word) {
            (_, "--") => return args.all(|path| relative_path(path, shell, true)),
            (true, "--oneline" | "--graph" | "--first-parent" | "--no-merges" | "--all") => (),
            (true, "-n" | "--max-count") => {
                if !args.next().is_some_and(number) {
                    return false;
                }
            }
            (false, "--stat" | "--name-only" | "--name-status" | "--patch") => (),
            (true, option) if option.strip_prefix("--max-count=").is_some_and(number) => (),
            (true, option) if option.strip_prefix('-').is_some_and(number) => (),
            (_, word) if !word.starts_with('-') && revision(word, shell) => (),
            _ => return false,
        }
    }
    true
}

fn number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn revision(word: &str, shell: Shell) -> bool {
    let index = word.find('~').unwrap_or(word.len());
    let (name, suffix) = word.split_at(index);
    let normalized = name.replace('/', if shell == Shell::Cmd { "\\" } else { "/" });
    (name == "@" || relative_path(&normalized, shell, false))
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'~')
}
