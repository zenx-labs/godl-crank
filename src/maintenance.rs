//! Periodic maintenance: checkpoint eligible miners, then close expired rounds.
//!
//! Runs in its own task on a fixed interval (`--close-interval` hours, 0 to
//! disable) so it never delays the time-critical per-round reset. Both phases
//! send many independent instructions through the bisecting batch sender, so a
//! single stale/raced instruction can't sink the rest of the batch.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use godl_api::prelude::*;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{signature::Keypair, signer::Signer};
use steel::Instruction;
use tokio::time::{sleep, Duration};

use crate::rpc::{get_board, get_clock, get_miners, get_rounds};
use crate::tx::send_instructions_bisecting;

/// Starting batch width for both phases (matches the upstream CLI).
const INITIAL_BATCH_SIZE: usize = 6;
/// Rough compute-unit budgets per instruction, used to size each tx.
const CHECKPOINT_CU_PER_IX: u32 = 60_000;
const CLOSE_CU_PER_IX: u32 = 40_000;

/// Run the maintenance loop forever on `interval_hours` cadence.
pub async fn run_maintenance_loop(
    rpc: Arc<RpcClient>,
    send_rpc: Arc<RpcClient>,
    payer: Arc<Keypair>,
    interval_hours: f64,
    priority_fee: u64,
) {
    let interval = Duration::from_secs_f64(interval_hours * 3600.0);
    println!(
        "[maintenance] enabled: checkpoint + close every {interval_hours}h ({}s)",
        interval.as_secs()
    );

    loop {
        if let Err(e) = maintenance_pass(&rpc, &send_rpc, &payer, priority_fee).await {
            eprintln!("[maintenance] pass error: {e:?}");
        }
        println!(
            "[maintenance] next pass in {interval_hours}h ({}s)",
            interval.as_secs()
        );
        sleep(interval).await;
    }
}

async fn maintenance_pass(
    rpc: &RpcClient,
    send_rpc: &RpcClient,
    payer: &Keypair,
    priority_fee: u64,
) -> Result<()> {
    println!("[maintenance] starting pass (checkpoint -> close)");
    checkpoint_eligible(rpc, send_rpc, payer, priority_fee).await?;
    close_eligible(rpc, send_rpc, payer, priority_fee).await?;
    Ok(())
}

/// Checkpoint every miner that hasn't checkpointed its last round and is inside
/// that round's 12h fee-collection window. Mirrors `cli checkpoint-rounds`.
async fn checkpoint_eligible(
    rpc: &RpcClient,
    send_rpc: &RpcClient,
    payer: &Keypair,
    priority_fee: u64,
) -> Result<()> {
    let clock = get_clock(rpc).await?;
    let miners = get_miners(rpc).await?;
    let rounds = get_rounds(rpc).await?;

    let expiry: HashMap<u64, u64> = rounds.iter().map(|(_, r)| (r.id, r.expires_at)).collect();
    println!(
        "[checkpoint] scanning {} miners against {} rounds",
        miners.len(),
        rounds.len()
    );

    let mut ixs: Vec<Instruction> = Vec::new();
    for (_, miner) in &miners {
        if miner.checkpoint_id >= miner.round_id {
            continue;
        }
        let Some(&expires_at) = expiry.get(&miner.round_id) else {
            continue;
        };
        let fee_start = expires_at.saturating_sub(TWELVE_HOURS_SLOTS);
        if clock.slot >= fee_start {
            ixs.push(godl_api::sdk::checkpoint(
                payer.pubkey(),
                miner.authority,
                miner.round_id,
            ));
        }
    }

    if ixs.is_empty() {
        println!("[checkpoint] nothing eligible");
        return Ok(());
    }
    println!("[checkpoint] {} miner(s) to checkpoint", ixs.len());
    let outcome = send_instructions_bisecting(
        send_rpc,
        payer,
        "checkpoint",
        ixs,
        priority_fee,
        CHECKPOINT_CU_PER_IX,
        INITIAL_BATCH_SIZE,
    )
    .await?;
    if outcome.dropped > 0 {
        eprintln!(
            "[checkpoint] WARNING: {} miner(s) could not be checkpointed this pass",
            outcome.dropped
        );
    }
    Ok(())
}

/// Close expired rounds whose rent this payer owns. Mirrors `cli close-rounds`.
async fn close_eligible(
    rpc: &RpcClient,
    send_rpc: &RpcClient,
    payer: &Keypair,
    priority_fee: u64,
) -> Result<()> {
    let clock = get_clock(rpc).await?;
    let board = get_board(rpc).await?;
    let rounds = get_rounds(rpc).await?;
    let payer_pubkey = payer.pubkey();

    println!("[close] scanning {} rounds", rounds.len());
    let mut ixs: Vec<Instruction> = Vec::new();
    for (_, round) in &rounds {
        if round.rent_payer == payer_pubkey
            && round.expires_at < clock.slot
            && round.id < board.round_id
        {
            ixs.push(godl_api::sdk::close(payer_pubkey, round.id, payer_pubkey));
        }
    }

    if ixs.is_empty() {
        println!("[close] nothing eligible");
        return Ok(());
    }
    println!("[close] {} round(s) to close", ixs.len());
    let outcome = send_instructions_bisecting(
        send_rpc,
        payer,
        "close",
        ixs,
        priority_fee,
        CLOSE_CU_PER_IX,
        INITIAL_BATCH_SIZE,
    )
    .await?;
    if outcome.dropped > 0 {
        eprintln!(
            "[close] WARNING: {} round(s) could not be closed this pass (already closed / not yet expired?)",
            outcome.dropped
        );
    }
    Ok(())
}
