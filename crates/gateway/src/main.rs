fn main() -> anyhow::Result<()> {
    if let Some(code) = hermes_gateway::owner::dispatch_role() { std::process::exit(code); }
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "gateway") {
        args.next();
    }
    hermes_gateway::owner::serve(args).await
}
