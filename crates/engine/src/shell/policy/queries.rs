//! Commands whose literal arguments request only a version or native system information.
use super::Shell;

pub(super) fn allows(name: &str, args: &[&str], shell: Shell) -> bool {
    let windows_name;
    let name = match shell {
        Shell::Cmd => {
            windows_name = name.to_ascii_lowercase();
            windows_name.strip_suffix(".exe").unwrap_or(&windows_name)
        }
        Shell::Posix => name,
    };
    // A missing installation is a native shell diagnostic for these command forms.
    // Leave resolution to the same sanitized PATH used for actual capture.
    match (name, args) {
        ("cargo" | "rustc" | "rg" | "node" | "npm" | "dotnet" | "git", ["--version"])
        | ("go" | "git", ["version"]) => true,
        ("where", programs) if shell == Shell::Cmd => bare_programs(programs),
        ("which", programs) if shell == Shell::Posix => bare_programs(programs),
        ("whoami" | "hostname", []) => true,
        ("ipconfig", []) | ("doskey", ["/history"]) => shell == Shell::Cmd,
        ("uname", [] | ["-a"]) | ("id", [] | ["-u" | "-un"]) | ("date", [] | ["-u"]) => {
            shell == Shell::Posix
        }
        _ => false,
    }
}

fn bare_programs(programs: &[&str]) -> bool {
    !programs.is_empty()
        && programs.iter().all(|name| {
            name.starts_with(|c: char| c.is_ascii_alphanumeric())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_queries_distinct_from_other_modes_of_the_same_program() {
        assert!(allows("WHERE.EXE", &["python", "git"], Shell::Cmd));
        assert!(allows("cargo", &["--version"], Shell::Posix));
        assert!(!allows("cargo", &["run"], Shell::Posix));
        assert!(!allows("where", &["/R", "."], Shell::Cmd));
        assert!(!allows("where", &["git"], Shell::Posix));
        assert!(!allows("ipconfig", &["/renew"], Shell::Cmd));
        assert!(!allows("CARGO", &["--version"], Shell::Posix));
    }
}
