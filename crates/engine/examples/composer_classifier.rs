//! Input-only driver for the stock composer differential test.
use hermes_engine::shell::Command;
use serde::Deserialize;
use std::{io, path::PathBuf};

#[derive(Deserialize)]
struct Request {
    cwd: PathBuf,
    commands: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let request: Request = serde_json::from_reader(io::stdin().lock())?;
    let cwd = request.cwd.canonicalize()?;
    let result: Vec<_> = request
        .commands
        .iter()
        .map(|command| {
            Command::parse(command)
                .and_then(|command| command.check(&cwd))
                .is_ok()
        })
        .collect();
    serde_json::to_writer(io::stdout().lock(), &result)?;
    Ok(())
}
