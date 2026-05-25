//! GODL crank bot.
//!
//! Two concurrent loops over a single user-provided RPC (no Helius Sender, no
//! Jito): a per-round `sample + reveal + reset_permissionless` cranker, and a
//! periodic `checkpoint + close` maintenance pass. Both are individually
//! error-isolated so the process keeps cranking through transient failures.

mod entropy;
mod maintenance;
mod reset;
mod rpc;
mod selection;
mod stats;
mod tx;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, native_token::LAMPORTS_PER_SOL,
    signature::read_keypair_file, signer::Signer,
};
use tokio::time::{sleep, Duration};

use crate::entropy::{EntropyClient, DEFAULT_HOSTS};
use crate::rpc::get_board;

/// Warn in healthchecks when the payer drops below this balance (lamports).
const LOW_BALANCE_LAMPORTS: u64 = 50_000_000; // 0.05 SOL

#[derive(Parser, Debug)]
#[command(
    name = "godl-crank",
    about = "Robust zero-downtime GODL crank bot",
    version
)]
struct Args {
    /// RPC endpoint used for both reads and transaction submission.
    #[arg(long, env = "RPC")]
    rpc: String,

    /// Path to the crank payer keypair (signs and pays fees).
    #[arg(long, env = "CRANK_KEYPAIR", value_name = "PATH")]
    keypair: PathBuf,

    /// Priority fee in micro-lamports per compute unit.
    #[arg(long, env = "PRIORITY_FEE", default_value_t = 10_000)]
    priority_fee: u64,

    /// Compute-unit limit for the reset transaction.
    #[arg(long, env = "COMPUTE_UNIT_LIMIT", default_value_t = 350_000)]
    compute_unit_limit: u32,

    /// Hours between checkpoint+close maintenance passes. 0 disables it.
    #[arg(long, env = "CLOSE_INTERVAL", default_value_t = 1.0)]
    close_interval: f64,

    /// Seconds between healthcheck heartbeat logs.
    #[arg(long, env = "HEALTH_INTERVAL", default_value_t = 60)]
    health_interval: u64,

    /// Comma-separated entropy hosts (primary first). Defaults to the built-in list.
    #[arg(long, env = "ENTROPY_HOSTS", value_delimiter = ',')]
    entropy_hosts: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = Args::parse();

    let payer = Arc::new(
        read_keypair_file(&args.keypair)
            .map_err(|e| anyhow::anyhow!("failed to read keypair {:?}: {e}", args.keypair))?,
    );

    // Two clients on the same endpoint: `processed` for fast polling/waits,
    // `confirmed` for the actual sends so we observe landed state reliably.
    let rpc = Arc::new(RpcClient::new_with_commitment(
        args.rpc.clone(),
        CommitmentConfig::processed(),
    ));
    let send_rpc = Arc::new(RpcClient::new_with_commitment(
        args.rpc.clone(),
        CommitmentConfig::confirmed(),
    ));

    let hosts = if args.entropy_hosts.is_empty() {
        DEFAULT_HOSTS.iter().map(|s| s.to_string()).collect()
    } else {
        args.entropy_hosts.clone()
    };
    let entropy = Arc::new(EntropyClient::new(hosts));
    let reset_stats = Arc::new(stats::ResetStats::new());

    startup_report(&args, &rpc, &payer, &entropy).await?;

    // Maintenance task (optional).
    if args.close_interval > 0.0 {
        let h = tokio::spawn(maintenance::run_maintenance_loop(
            rpc.clone(),
            send_rpc.clone(),
            payer.clone(),
            args.close_interval,
            args.priority_fee,
        ));
        // If it ever panics, log it — the reset loop keeps running regardless.
        tokio::spawn(async move {
            if let Err(e) = h.await {
                eprintln!("[maintenance] task ended unexpectedly: {e:?}");
            }
        });
    } else {
        println!("[maintenance] disabled (--close-interval 0)");
    }

    // Healthcheck heartbeat task.
    tokio::spawn(run_healthcheck_loop(
        rpc.clone(),
        payer.clone(),
        reset_stats.clone(),
        args.health_interval,
    ));

    // Reset loop runs on the main task and never returns.
    reset::run_reset_loop(
        rpc,
        send_rpc,
        payer,
        entropy,
        reset_stats,
        args.priority_fee,
        args.compute_unit_limit,
    )
    .await;

    Ok(())
}

/// One-shot startup banner: confirms config, connectivity, and balance.
async fn startup_report(
    args: &Args,
    rpc: &RpcClient,
    payer: &solana_sdk::signature::Keypair,
    entropy: &EntropyClient,
) -> Result<()> {
    println!("=== godl-crank starting ===");
    println!("[setup] RPC: {}", args.rpc);
    println!("[setup] payer: {}", payer.pubkey());
    println!(
        "[setup] priority fee: {} µLamports/CU, reset CU limit: {}",
        args.priority_fee, args.compute_unit_limit
    );
    println!(
        "[setup] maintenance: {}",
        if args.close_interval > 0.0 {
            format!("every {}h", args.close_interval)
        } else {
            "disabled".to_string()
        }
    );
    println!("[setup] entropy hosts: {}", entropy.hosts().join(", "));

    let version = rpc
        .get_version()
        .await
        .context("RPC connectivity check failed (get_version)")?;
    println!("[setup] RPC reachable, solana-core {}", version.solana_core);

    let balance = rpc.get_balance(&payer.pubkey()).await?;
    println!(
        "[setup] payer balance: {:.6} SOL",
        balance as f64 / LAMPORTS_PER_SOL as f64
    );
    if balance < LOW_BALANCE_LAMPORTS {
        eprintln!(
            "[setup] WARNING: low balance ({:.6} SOL) — fund the payer to keep cranking",
            balance as f64 / LAMPORTS_PER_SOL as f64
        );
    }

    match get_board(rpc).await {
        Ok(b) => println!(
            "[setup] board: round {}, start_slot {}, end_slot {}",
            b.round_id, b.start_slot, b.end_slot
        ),
        Err(e) => eprintln!("[setup] WARNING: could not read board: {e}"),
    }
    println!("=== setup complete, entering crank loops ===");
    Ok(())
}

/// Periodic heartbeat so operators can see the bot is alive and healthy.
async fn run_healthcheck_loop(
    rpc: Arc<RpcClient>,
    payer: Arc<solana_sdk::signature::Keypair>,
    reset_stats: Arc<stats::ResetStats>,
    interval_secs: u64,
) {
    let interval = Duration::from_secs(interval_secs.max(5));
    loop {
        sleep(interval).await;
        let slot = rpc.get_slot().await;
        let board = get_board(&rpc).await;
        let balance = rpc.get_balance(&payer.pubkey()).await;

        let slot_str = slot
            .map(|s| s.to_string())
            .unwrap_or_else(|e| format!("ERR({e})"));
        let board_str = match board {
            Ok(b) => format!("round={} end_slot={}", b.round_id, b.end_slot),
            Err(e) => format!("board=ERR({e})"),
        };
        let bal_str = match balance {
            Ok(bal) => {
                if bal < LOW_BALANCE_LAMPORTS {
                    eprintln!(
                        "[health] WARNING: low payer balance ({:.6} SOL)",
                        bal as f64 / LAMPORTS_PER_SOL as f64
                    );
                }
                format!("{:.6} SOL", bal as f64 / LAMPORTS_PER_SOL as f64)
            }
            Err(e) => format!("ERR({e})"),
        };
        println!(
            "[health] slot={slot_str} {board_str} balance={bal_str} | resets {}",
            reset_stats.summary()
        );
    }
}
