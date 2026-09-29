//! `manus-agent`: the agent wallet on its own, for building and auditing from source.
//! The official `manus` binary exposes the same commands as `manus agent …`.

use clap::Parser;
use manus_agent_wallet::cli::{self, AgentAction};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "manus-agent", version)]
#[command(
    about = "Your agent's own Solana wallet: budgeted spending, invoices between agents, Touch ID above the budget"
)]
struct Args {
    /// Wallet name under ~/.manus
    #[arg(short, long, global = true, default_value = "default")]
    name: String,

    #[command(subcommand)]
    action: AgentAction,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Several TLS stacks share rustls; pick one crypto provider explicitly.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // stdout belongs to MCP; logs go to stderr.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,hyper=error,sqlx=error")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let args = Args::parse();
    cli::run(&args.name, &args.action).await
}
