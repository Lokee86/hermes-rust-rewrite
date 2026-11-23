//! Classification of read-only shell path operands, separate from write targets.
use super::{Shell, windows_path_component};

pub(super) fn local_read_path(path: &str, shell: Shell, allow_cwd: bool) -> bool {
    if shell == Shell::Cmd
        && let Some((parent, file)) = path.split_once('/')
    {
        // Native readers can interpret a slash as switch syntax. Limit this
        // spelling to a relative directory and an ordinary filename with an extension.
        return file
            .split_once('.')
            .is_some_and(|(stem, ext)| stem.chars().count() > 1 && !ext.is_empty())
            && !file.contains('\\')
            && classify(parent, shell, false, false)
            && classify(file, shell, false, false);
    }
    classify(path, shell, allow_cwd, false)
}

// CMD's dir handles wildcards itself; unlike POSIX glob expansion, operands
// cannot be converted into a different program's options by filename expansion.
pub(super) fn local_dir_path(path: &str) -> bool {
    classify(path, Shell::Cmd, true, true)
}

pub(super) fn read_path(path: &str, shell: Shell) -> bool {
    read_from_start(path, shell, false)
}

pub(super) fn list_path(path: &str, shell: Shell) -> bool {
    read_from_start(path, shell, true)
}

fn read_from_start(path: &str, shell: Shell, listing: bool) -> bool {
    if path.is_empty() || path.starts_with('-') {
        return false;
    }
    let separator = |ch| ch == '/' || (shell == Shell::Cmd && ch == '\\');
    let (path, rooted) = match (shell, path.as_bytes()) {
        (Shell::Cmd, [drive, b':', b'\\' | b'/', ..]) if drive.is_ascii_alphabetic() => {
            (&path[3..], true)
        }
        (Shell::Cmd, [b'\\', ..]) | (Shell::Posix, [b'/', ..]) => (&path[1..], true),
        _ => (path, false),
    };
    if path.starts_with(separator) {
        return false;
    }
    if path.is_empty() {
        return rooted && listing;
    }
    // Native CMD utilities can treat a forward slash as a switch. Preserve
    // the ordinary relative filename spelling without opening switch-shaped
    // path components or forwarding slash switches to DIR.
    if shell == Shell::Cmd && path.contains('/') {
        if listing || (!rooted && !local_read_path(path, shell, false)) {
            return false;
        }
        if path.split('/').skip(1).any(|part| {
            part.split(['/', '\\'])
                .next()
                .is_none_or(|name| name.chars().count() < 2)
        }) {
            return false;
        }
    }

    let path = if listing {
        path.strip_suffix(separator).unwrap_or(path)
    } else {
        path
    };
    let mut parts = path.split(separator).peekable();
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        if matches!(part, "." | "..") {
            if last && !listing {
                return false;
            }
            continue;
        }
        if !ordinary_part(
            part,
            shell,
            last,
            listing,
            shell == Shell::Cmd && listing && last,
        ) || protected_part(part)
            || (shell == Shell::Posix && matches!(part, "dev" | "proc" | "sys"))
        {
            return false;
        }
    }
    true
}

fn protected_part(part: &str) -> bool {
    let name = part.to_lowercase();
    matches!(
        name.as_str(),
        ".aws"
            | ".azure"
            | ".codex"
            | ".credentials"
            | ".docker"
            | ".git"
            | ".gnupg"
            | ".hermes"
            | ".kube"
            | ".ssh"
            | ".netrc"
            | ".npmrc"
            | ".pypirc"
            | ".secrets"
            | "config.yaml"
            | "credentials"
            | "credentials.json"
            | "environ"
            | "gshadow"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
            | "id_rsa"
            | "shadow"
    ) || part.to_uppercase() == "CONFIG.YAML"
        || matches!(name.as_str(), ".env" | ".envrc")
        || name.starts_with(".env.")
        || name.starts_with(".env-")
        || [".key", ".pem", ".pfx", ".p12"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn ordinary_part(part: &str, shell: Shell, last: bool, hidden: bool, wildcard: bool) -> bool {
    ((last && part.eq_ignore_ascii_case(".gitignore"))
        || (hidden
            && part
                .strip_prefix('.')
                .is_some_and(|name| name.starts_with(char::is_alphanumeric)))
        || part.starts_with(|ch: char| {
            ch.is_alphanumeric() || ch == '_' || (wildcard && matches!(ch, '*' | '?'))
        }))
        && part.chars().all(|ch| {
            ch.is_alphanumeric()
                || matches!(ch, ' ' | '_' | '-' | '.' | '+')
                || (wildcard && matches!(ch, '*' | '?'))
        })
        && (shell != Shell::Cmd || windows_path_component(part))
}
fn classify(path: &str, shell: Shell, allow_cwd: bool, wildcard: bool) -> bool {
    let separator = match shell {
        Shell::Posix => '/',
        Shell::Cmd => '\\',
    };
    if path.is_empty() || path.starts_with(separator) || path.starts_with('-') {
        return false;
    }
    if allow_cwd
        && (path == "."
            || path
                .strip_prefix('.')
                .is_some_and(|rest| rest.starts_with(separator) && rest.len() == 1))
    {
        return true;
    }
    let path = path
        .strip_prefix('.')
        .and_then(|rest| rest.strip_prefix(separator))
        .unwrap_or(path);
    let path = if allow_cwd {
        path.strip_suffix(separator).unwrap_or(path)
    } else {
        path
    };
    let mut components = path.split(separator).peekable();
    while let Some(part) = components.next() {
        let last = components.peek().is_none();
        let wildcard = wildcard && last;
        let valid = ordinary_part(part, shell, last, false, wildcard)
            && !part.eq_ignore_ascii_case("config.yaml")
            && (shell != Shell::Cmd || part.is_ascii() || part.to_uppercase() != "CONFIG.YAML");
        if !valid {
            return false;
        }
    }
    true
}

pub(super) fn dir_flag(arg: &str) -> bool {
    let arg = arg.to_ascii_lowercase();
    if ["/a", "/b", "/l", "/n", "/o", "/w"].contains(&arg.as_str()) {
        return true;
    }
    let (flags, alphabet) = if let Some(flags) = arg.strip_prefix("/a:") {
        (flags, "drahsli-")
    } else if let Some(flags) = arg.strip_prefix("/o:") {
        (flags, "nesdg-")
    } else {
        return false;
    };
    !flags.is_empty() && flags.chars().all(|flag| alphabet.contains(flag))
}
