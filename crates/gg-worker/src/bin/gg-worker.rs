//! `gg-worker`: contributor agent.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gg_worker::agent::Agent;
use gg_worker::config::WorkerConfig;

#[derive(Parser)]
#[command(name = "gg-worker", about = "GPU Goblin contributor agent")]
struct Cli {
    /// TOML configuration (environment `GG_*` variables override it).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the key (if absent) and print the public key.
    Init,
    /// Print the public key.
    Identity,
    /// Check local protections and engine availability.
    Doctor {
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Report what the local network offers for reachability: default gateway, NAT-PMP, UPnP-IGD.
    /// With `--map`, it asks for a short-lived mapping and removes it immediately.
    NatProbe {
        /// Gateway to question (default: the IPv4 default route).
        #[arg(long)]
        gateway: Option<std::net::Ipv4Addr>,
        /// Ask for a mapping of this local port, then delete it.
        #[arg(long)]
        map: Option<u16>,
        /// Lease requested for that test mapping, in seconds.
        #[arg(long, default_value_t = 120)]
        lease: u32,
    },
    /// Register, wait for authorization, then claim and execute tasks until Ctrl-C.
    Run {
        /// Stop after this many attempts (for demos and tests).
        #[arg(long)]
        max_tasks: Option<usize>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let cfg = match WorkerConfig::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            std::process::exit(2);
        }
    };
    let result: anyhow::Result<()> = match cli.cmd {
        Cmd::Init | Cmd::Identity => (|| {
            let key = gg_protocol::keys::SecretKey::load_or_generate(&cfg.key_path)?;
            println!("{}", key.public_key());
            Ok(())
        })(),
        Cmd::NatProbe {
            gateway,
            map,
            lease,
        } => gg_worker::nat_probe::run(gateway, map, lease).await,
        Cmd::Doctor { json } => {
            let report = gg_worker::doctor::run(&cfg);
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print!("{}", gg_worker::doctor::render(&report));
            }
            if report.may_start {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Cmd::Run { max_tasks } => {
            async {
                let report = gg_worker::doctor::run(&cfg);
                eprint!("{}", gg_worker::doctor::render(&report));
                anyhow::ensure!(report.may_start, "doctor refused the current configuration");
                let (tx, rx) = tokio::sync::watch::channel(false);
                tokio::spawn(async move {
                    gg_client::signals::terminated().await;
                    tracing::info!("shutdown requested");
                    let _ = tx.send(true);
                });
                let mut agent = Agent::new(cfg).await?;
                agent.run(rx, max_tasks).await?;
                tracing::info!(attempts = agent.processed.len(), "agent stopped");
                Ok(())
            }
            .await
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
