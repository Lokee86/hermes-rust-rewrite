//! Environment inherited by a local composer command.
use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

pub(super) use crate::privacy::redact_capture as redact;

const MANAGED_PREFIXES: &[&str] = &[
    "HERMES_",
    "_HERMES_",
    "TERMINAL_",
    "AUXILIARY_",
    "API_SERVER_",
    "WEBHOOK_",
    "GEMINI_",
    "GOOGLE_GENAI_",
    "AI_GATEWAY_",
    "MCP_",
    "PLUGIN_",
    "GATEWAY_",
    "OPENAI_",
    "OPENROUTER_",
    "ANTHROPIC_",
    "AZURE_OPENAI_",
    "VERTEX_",
    "OLLAMA_",
    "TELEGRAM_",
    "DISCORD_",
    "SLACK_",
    "WHATSAPP_",
    "SIGNAL_",
    "MATRIX_",
    "BUZZ_",
    "EMAIL_",
    "FEISHU_",
    "LARK_",
    "WEIXIN_",
    "WECHAT_",
    "DINGTALK_",
    "MSGRAPH_",
    "MATTERMOST_",
    "QQ_",
    "AGENTMAIL_",
    "TWILIO_",
    "VERCEL_",
    "MODAL_",
    "DAYTONA_",
    "LD_",
    "DYLD_",
    "BASH_FUNC_",
];

const MANAGED_NAMES: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GITHUB_APP_ID",
    "GITHUB_APP_INSTALLATION_ID",
    "GITHUB_APP_PRIVATE_KEY_PATH",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AWS_BEARER_TOKEN_BEDROCK",
    "LLM_MODEL",
    "FIRECRAWL_API_URL",
    "BASH_ENV",
    "BASHOPTS",
    "CDPATH",
    "ENV",
    "NODE_OPTIONS",
    "PERL5OPT",
    "PROMPT_COMMAND",
    "PS4",
    "PYTHONHOME",
    "PYTHONSTARTUP",
    "RUBYOPT",
    "SHELLOPTS",
    "ZDOTDIR",
    "CONDA_PREFIX",
    "VIRTUAL_ENV",
    "PWD",
    "OLDPWD",
];

const CREDENTIAL_SUFFIXES: &[&str] = &[
    "_API_KEY",
    "_APIKEY",
    "_ACCESS_KEY",
    "_ACCESS_KEY_ID",
    "_AUTH",
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_PASSWD",
    "_PRIVATE_KEY",
    "_CREDENTIAL",
    "_CREDENTIALS",
];

pub(super) struct Environment {
    pub(super) values: Vec<(OsString, OsString)>,
    pub(super) programs: Vec<String>,
}

pub(super) fn snapshot(cwd: &Path, _shell: &Path, script: &str) -> Environment {
    static LAUNCH_KEYS: LazyLock<HashSet<String>> = LazyLock::new(launch_keys);
    let mut values = Vec::new();
    let mut path = Vec::new();
    #[cfg(unix)]
    path.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    #[cfg(windows)]
    if let Some(system) = _shell.parent() {
        path.push(system.to_owned());
    }
    for (key, value) in env::vars_os() {
        let name = key.to_string_lossy();
        if excluded(&name, &LAUNCH_KEYS) {
            continue;
        }
        if name.eq_ignore_ascii_case("PATH") {
            for dir in env::split_paths(&value) {
                if dir.is_absolute() && !dir.starts_with(cwd) && !path.contains(&dir) {
                    path.push(dir);
                }
            }
            continue;
        }
        if cfg!(windows)
            && ["COMSPEC", "PATHEXT", "NoDefaultCurrentDirectoryInExePath"]
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            continue;
        }
        values.push((key, value));
    }
    if let Ok(joined) = env::join_paths(&path) {
        values.push(("PATH".into(), joined));
    }
    #[cfg(windows)]
    {
        values.push(("COMSPEC".into(), _shell.as_os_str().to_owned()));
        values.push(("PATHEXT".into(), ".EXE".into()));
        values.push(("NoDefaultCurrentDirectoryInExePath".into(), "1".into()));
    }
    let mut programs = Vec::new();
    // Inspect only names present in this command. Keep both the bare spelling and
    // matching absolute paths so an explicitly chosen interpreter can be used.
    let spelling = script.to_ascii_lowercase();
    let mut seen = HashSet::new();
    for word in
        spelling.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')))
    {
        let name = word.strip_suffix(".exe").unwrap_or(word);
        let numbered_python = name
            .strip_prefix("python3.")
            .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()));
        // These names also occur in third-party tool directories. Admit them
        // only when the native shell's system directory supplies their executable.
        let windows_system_utility = cfg!(windows) && matches!(name, "find" | "fc" | "timeout");
        if !(numbered_python
            || windows_system_utility
            || matches!(name, "python" | "python3" | "findstr" | "rg" | "git"))
            || !seen.insert(name)
        {
            continue;
        }
        let filename = format!("{name}{}", env::consts::EXE_SUFFIX);
        let paths = path
            .iter()
            .filter(|dir| !windows_system_utility || Some(dir.as_path()) == _shell.parent())
            .map(|dir| dir.join(&filename))
            .filter(|file| file.is_file());
        let mut found = false;
        for executable in paths {
            if let Some(absolute) = executable.to_str() {
                programs.push(absolute.to_owned());
                found = true;
            }
        }
        if found {
            programs.push(name.to_owned());
            if filename != name {
                programs.push(filename);
            }
        }
    }
    Environment { values, programs }
}

fn excluded(name: &str, launch_keys: &HashSet<String>) -> bool {
    let name = name.to_ascii_uppercase();
    if (launch_keys.contains(&name) && !is_operator_global(&name))
        || MANAGED_NAMES.contains(&name.as_str())
        || (cfg!(windows) && (name == "CD" || name.starts_with('=')))
    {
        return true;
    }

    // Stock permits the user's AWS operator tools and first-party Claude CLI login.
    if name.starts_with("AWS_")
        || matches!(
            name.as_str(),
            "CLAUDE_CODE_OAUTH_TOKEN" | "SLACK_USER_TOKEN"
        )
    {
        return false;
    }

    MANAGED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
        || matches!(
            name.as_str(),
            "API_KEY" | "APIKEY" | "TOKEN" | "SECRET" | "PASSWORD" | "PASSWD" | "CREDENTIAL"
        )
}

fn is_operator_global(name: &str) -> bool {
    name.starts_with("LC_")
        || matches!(
            name,
            "PATH"
                | "HOME"
                | "USER"
                | "USERPROFILE"
                | "SYSTEMROOT"
                | "WINDIR"
                | "COMSPEC"
                | "PATHEXT"
                | "LANG"
                | "TZ"
                | "SHELL"
                | "TMP"
                | "TEMP"
                | "TMPDIR"
                | "PYTHONPATH"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
        )
}

fn launch_keys() -> HashSet<String> {
    // Native profiles do not hydrate dotenv into the owner. A stock launcher can
    // still supply its profile's exported values before starting the native owner.
    let stock_home = env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".hermes")));
    [stock_home, crate::Session::default_dir().ok()]
        .into_iter()
        .flatten()
        .map(|home| home.join(".env"))
        .filter(|path| path.is_file())
        .filter_map(|path| fs::read_to_string(path).ok())
        .flat_map(|text| dotenv_keys(&text).collect::<Vec<_>>())
        .collect()
}

fn dotenv_keys(text: &str) -> impl Iterator<Item = String> + '_ {
    text.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let line = line
                .strip_prefix("export")
                .filter(|rest| rest.starts_with(char::is_whitespace))
                .map_or(line, str::trim_start);
            let (name, _) = line.split_once('=')?;
            let name = name.trim_end().trim_matches('\'');
            (!name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
            .then(|| name.to_ascii_uppercase())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_launch_credentials_and_startup_hooks_while_retaining_operator_environment() {
        let launch = dotenv_keys(
            "\u{feff}export launch_flag = test\n'CUSTOM_SETTING'=x\npath=global\n# NOTE=y",
        )
        .collect();
        for name in [
            "launch_flag",
            "CUSTOM_SETTING",
            "openai_api_key",
            "HERMES_RUST_GATEWAY_TOKEN",
            "terminal_env",
            "MCP_EXAMPLE_ENDPOINT",
            "PLUGIN_ENDPOINT",
            "PRIVATE_SERVICE_SECRET",
            "GITHUB_TOKEN",
            "DISCORD_ALLOWED_USERS",
            "AUXILIARY_UTILITY_BASE_URL",
            "AWS_BEARER_TOKEN_BEDROCK",
            "BASH_ENV",
            "BASH_FUNC_builtin%%",
            "LD_PRELOAD",
            "PWD",
        ] {
            assert!(excluded(name, &launch), "{name}");
        }
        for name in [
            "Path",
            "HOME",
            "LANG",
            "TMP",
            "TERM",
            "PYTHONPATH",
            "SSH_AUTH_SOCK",
            "AWS_ACCESS_KEY_ID",
            "AWS_SESSION_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "SLACK_USER_TOKEN",
            "USER_SETTING",
            "DEMO_ALLOWED_SENDER",
        ] {
            assert!(!excluded(name, &launch), "{name}");
        }
    }
}
