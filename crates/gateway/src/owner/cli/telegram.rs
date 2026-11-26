use super::{Session, discover};
use crate::client::{ClientOptions, GatewayClient};
use anyhow::{Context, Result, anyhow, bail, ensure};
use hermes_telegram::{Settings, Store, Token, parse_allowlist};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

const HELP: &str = "Headless Telegram for a native profile

hermes-gateway gateway telegram configure [--data-dir PATH] --bot-token-file PATH --allow-id ID [--allow-id ID ...]
hermes-gateway gateway telegram status [--data-dir PATH]
hermes-gateway gateway telegram resolve-delivery [--data-dir PATH] --bot-id ID --update-id ID --attempt N --resolution delivered|discard
hermes-gateway gateway telegram resolve-execution [--data-dir PATH] --bot-id ID --update-id ID";

pub(super) async fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let command = args.next().unwrap_or_else(|| "--help".to_owned());
    if matches!(command.as_str(), "--help" | "-h") {
        println!("{HELP}");
        return Ok(());
    }
    ensure!(
        matches!(
            command.as_str(),
            "configure" | "status" | "resolve-delivery" | "resolve-execution"
        ),
        "Unknown Telegram command. Use --help"
    );
    let mut root = None;
    let mut token_path = None;
    let mut allowed = Vec::new();
    let mut bot = None;
    let mut update = None;
    let mut attempt = None;
    let mut resolution = None;
    while let Some(option) = args.next() {
        if matches!(option.as_str(), "--help" | "-h") {
            println!("{HELP}");
            return Ok(());
        }
        let value = args
            .next()
            .with_context(|| format!("{option} requires a value"))?;
        match (command.as_str(), option.as_str()) {
            (_, "--data-dir") => root = Some(PathBuf::from(value)),
            ("configure", "--bot-token-file") => token_path = Some(PathBuf::from(value)),
            ("configure", "--allow-id") => allowed.push(value),
            ("resolve-delivery" | "resolve-execution", "--bot-id") => {
                bot = Some(value.parse::<i64>().context("Invalid bot ID")?)
            }
            ("resolve-delivery" | "resolve-execution", "--update-id") => {
                update = Some(value.parse::<i64>().context("Invalid update ID")?)
            }
            ("resolve-delivery", "--attempt") => {
                attempt = Some(value.parse::<i64>().context("Invalid attempt")?)
            }
            ("resolve-delivery", "--resolution") => resolution = Some(value),
            _ => bail!("Unknown option for {command}: {option}"),
        }
    }
    let root = match root {
        Some(root) => root,
        None => Session::default_dir()?,
    };
    if command == "configure" {
        let path = token_path.context("--bot-token-file is required")?;
        let token = credential(&path)?;
        let allowed = parse_allowlist(&allowed)?;
        fs::create_dir_all(&root).context("Could not create the profile data directory")?;
        let root = fs::canonicalize(&root)?;
        let _owner = super::super::try_profile_lock(&root)?
            .context("Stop the profile gateway before configuring Telegram")?;
        let store = Store::new(&root);
        let bot = store
            .load_settings()
            .map_err(anyhow::Error::msg)?
            .filter(|saved| saved.token.as_ref() == Some(&token))
            .and_then(|saved| saved.bot);
        store
            .save_settings(&Settings {
                version: 1,
                token: Some(token),
                allowed,
                bot,
                enabled: true,
                agent_replies: true,
            })
            .map_err(anyhow::Error::msg)?;
        println!("{}", json!({"enabled":true,"agent_replies":true}));
        return Ok(());
    }

    let root = fs::canonicalize(&root).context("Could not open the profile data directory")?;
    let owner = discover(&root)
        .await?
        .context("Start the local profile gateway first")?;
    ensure!(
        owner.is_native(),
        "Headless Telegram requires the native profile gateway"
    );
    let client = GatewayClient::new(ClientOptions::default());
    owner.connect(&client).await?;
    let result =
        match command.as_str() {
            "status" => client.request("telegram.status", json!({})).await?,
            "resolve-delivery" => {
                let bot = bot
                    .filter(|id| *id > 0)
                    .context("--bot-id must be positive")?;
                let update = update
                    .filter(|id| *id >= 0)
                    .context("--update-id must be nonnegative")?;
                let attempt = attempt
                    .filter(|n| *n > 0)
                    .context("--attempt must be positive")?;
                let resolution = resolution
                    .filter(|action| matches!(action.as_str(), "delivered" | "discard"))
                    .context("--resolution must be delivered or discard")?;
                client.request("telegram.resolve_delivery", json!({
                "bot_id":bot,"update_id":update,"attempt":attempt,"resolution":resolution,
            })).await?
            }
            "resolve-execution" => {
                let bot = bot
                    .filter(|id| *id > 0)
                    .context("--bot-id must be positive")?;
                let update = update
                    .filter(|id| *id >= 0)
                    .context("--update-id must be nonnegative")?;
                resolve_execution(&client, bot, update).await?
            }
            _ => unreachable!("validated Telegram command"),
        };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn resolve_execution(client: &GatewayClient, bot: i64, update: i64) -> Result<Value> {
    let status = client.request("telegram.status", json!({})).await?;
    ensure!(
        status["bot"]["id"] == bot,
        "Bot ID does not match the saved connection"
    );
    let issue = status["work"]["issues"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|issue| issue["update_id"] == update && issue["stage"] == "turn_unknown")
        .context("No uncertain Telegram execution matches this update")?;
    let session = issue["session_id"]
        .as_str()
        .context("Telegram conversation is unavailable")?;
    let admission = issue["admission_id"]
        .as_str()
        .context("Telegram admission is unavailable")?;
    let generation = issue["execution_generation"]
        .as_u64()
        .context("Telegram admission is unavailable")?;
    client
        .request(
            "session.resume",
            json!({"session_id":session,"omit_messages":true}),
        )
        .await?;
    client
        .request(
            "prompt.resolve_unknown",
            json!({
                "session_id":session,"admission_id":admission,"execution_generation":generation,
            }),
        )
        .await?;
    Ok(json!({"resolved": true, "update_id": update}))
}

fn credential(path: &Path) -> Result<Token> {
    let meta =
        fs::symlink_metadata(path).context("Could not inspect the Telegram bot token file")?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Telegram bot token file must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "Make the Telegram bot token file private with chmod 600"
        );
    }
    let limit = 1024;
    let mut value = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut value)
        .context("Could not read the Telegram bot token file")?;
    if value.len() as u64 > limit {
        return Err(anyhow!("Telegram bot token file is too large"));
    }
    Ok(Token::parse(&value)?)
}
