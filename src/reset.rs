//! Per-round crank loop: sample + reveal + reset_permissionless.
//!
//! Timing mirrors the legacy bot: wait for the round's mining to end, prepare
//! the entropy seed and top-miner selection, then wait for the intermission to
//! clear (`end_slot + INTERMISSION_SLOTS`) before submitting. One atomic
//! transaction holds all three instructions, sent over the plain RPC.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use godl_api::prelude::*;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signature::Signature, signer::Signer};
use tokio::time::{sleep, Duration};

use crate::entropy::EntropyClient;
use crate::rpc::{get_board, get_config, get_round, get_var};
use crate::selection::select_top_miner;
use crate::stats::ResetStats;
use crate::tx::send_fire;

/// Poll interval while waiting on slots / between submit retries.
const SLEEP_MS: u64 = 400;
/// Times we (re-)fire the reset before declaring the round a failure.
const NUM_RETRIES: u64 = 4;
/// Poll iterations (× SLEEP_MS) we watch for an outcome after each fire.
const POLL_ITERS: u64 = 12;
/// If we're more than this many slots short of the target, something is off
/// (e.g. `end_slot == u64::MAX`); stop waiting and re-evaluate the board.
const MAX_WAIT_SLOTS: u64 = 100_000;

/// What happened to a round we tried to reset.
enum Outcome {
    /// Our transaction confirmed and advanced the round.
    Won(Signature),
    /// The round advanced, but not by us.
    Lost(u64),
}

/// Run the reset loop forever. Each iteration is independently error-handled so
/// the loop never dies — that's the whole point of "zero downtime".
pub async fn run_reset_loop(
    rpc: Arc<RpcClient>,
    send_rpc: Arc<RpcClient>,
    payer: Arc<Keypair>,
    entropy: Arc<EntropyClient>,
    stats: Arc<ResetStats>,
    priority_fee: u64,
    cu_limit: u32,
) {
    loop {
        match reset_iteration(
            &rpc,
            &send_rpc,
            &payer,
            &entropy,
            &stats,
            priority_fee,
            cu_limit,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[reset] iteration error: {e:?}");
                sleep(Duration::from_millis(SLEEP_MS)).await;
            }
        }
    }
}

async fn reset_iteration(
    rpc: &RpcClient,
    send_rpc: &RpcClient,
    payer: &Keypair,
    entropy: &EntropyClient,
    stats: &ResetStats,
    priority_fee: u64,
    cu_limit: u32,
) -> Result<()> {
    let board = get_board(rpc).await?;

    // A freshly-reset round has `end_slot == u64::MAX` until the first deploy
    // sets it. Nothing to reset yet — wait without hammering the chain.
    if board.end_slot == u64::MAX {
        println!(
            "[reset] round {} open, no deploys yet (end_slot unset); idling",
            board.round_id
        );
        sleep(Duration::from_secs(2)).await;
        return Ok(());
    }

    let target_slot = board.end_slot.saturating_add(INTERMISSION_SLOTS);
    println!(
        "[reset] round {} — mining ends @ slot {}, reset target @ slot {}",
        board.round_id, board.end_slot, target_slot
    );

    // Wait for mining to end, then prepare.
    let mining_end_slot = board.end_slot + 1;
    println!("[reset] waiting for mining to end at slot {mining_end_slot}");
    wait_until_slot(rpc, mining_end_slot, "reset:mining-end").await?;

    let prep = Instant::now();
    let config = get_config(rpc).await?;

    // Bail if the round already advanced (someone else reset it) during the wait.
    let board_now = get_board(rpc).await?;
    if board_now.round_id != board.round_id {
        println!(
            "[reset] round advanced {} -> {} before prep; restarting",
            board.round_id, board_now.round_id
        );
        return Ok(());
    }

    let round = get_round(rpc, board.round_id).await?;
    let var_address = config.var_address;
    let var = get_var(rpc, var_address).await?;

    let (seed, host) = entropy.get_seed(var_address).await?;
    println!("[reset] seed fetched from {host} (var {var_address})");

    let top = select_top_miner(rpc, &round, &var, seed).await?;
    let top_authority = match &top {
        Some(t) => {
            println!(
                "[reset] top miner {} on square {} (sample {})",
                t.authority, t.winning_square, t.sample
            );
            t.authority
        }
        None => {
            println!("[reset] split / empty winning square — passing default top miner");
            Pubkey::default()
        }
    };

    let sample_ix = entropy_api::sdk::sample(payer.pubkey(), var_address);
    let reveal_ix = entropy_api::sdk::reveal(payer.pubkey(), var_address, seed);
    let reset_ix = godl_api::sdk::reset_permissionless(
        payer.pubkey(),
        config.fee_collector,
        board.round_id,
        top_authority,
        var_address,
    );
    let ixs = [sample_ix, reveal_ix, reset_ix];

    // Pre-fetch the blockhash NOW so firing at the target slot is a single
    // sendTransaction with no round-trip in the critical path. It stays valid
    // well past our short prep→fire window (~150 slot lifetime).
    let blockhash = send_rpc.get_latest_blockhash().await?;

    println!(
        "[reset] prepared in {:?}; waiting for target slot {}",
        prep.elapsed(),
        target_slot
    );
    wait_until_slot(rpc, target_slot, "reset:submit").await?;

    let round_id = board.round_id;
    let next_round_id = round_id + 1;
    let mut our_sigs: Vec<Signature> = Vec::new();

    // Fire fast, then poll for the outcome. Re-fire each attempt until we either
    // see one of our signatures confirm (won) or the round advance (lost).
    for attempt in 1..=NUM_RETRIES {
        match send_fire(send_rpc, payer, &ixs, priority_fee, cu_limit, blockhash).await {
            Ok(sig) => {
                println!("[reset] round {round_id} attempt {attempt}/{NUM_RETRIES}: fired {sig}");
                our_sigs.push(sig);
            }
            Err(e) => eprintln!("[reset] round {round_id} attempt {attempt}/{NUM_RETRIES}: {e}"),
        }

        if let Some(outcome) = poll_outcome(rpc, &our_sigs, next_round_id).await {
            return finish(stats, round_id, outcome);
        }
    }

    // Nothing resolved. If the round did advance by now, it was someone else;
    // otherwise it genuinely failed (and we record it as such).
    if let Some(outcome) = poll_outcome(rpc, &our_sigs, next_round_id).await {
        return finish(stats, round_id, outcome);
    }
    stats.record_failure();
    println!(
        "[reset] round {round_id} FAILED — did not advance. [stats] {}",
        stats.summary()
    );
    Err(anyhow!(
        "round {round_id} did not advance after {NUM_RETRIES} attempts"
    ))
}

/// Watch our signatures and the board for `POLL_ITERS` ticks. Returns `Some`
/// once an outcome is known. Our own confirmation always wins over a bare
/// board advance, so a win is never misreported as a loss.
async fn poll_outcome(
    rpc: &RpcClient,
    our_sigs: &[Signature],
    next_round_id: u64,
) -> Option<Outcome> {
    for _ in 0..POLL_ITERS {
        sleep(Duration::from_millis(SLEEP_MS)).await;

        // Did one of ours land successfully?
        if let Some(sig) = first_confirmed(rpc, our_sigs).await {
            return Some(Outcome::Won(sig));
        }

        // Did the round advance (someone beat us, or our tx confirms a tick later)?
        if let Ok(b) = get_board(rpc).await {
            if b.round_id >= next_round_id {
                // Grace: give our own sigs a moment to surface before calling it a loss.
                for _ in 0..3 {
                    if let Some(sig) = first_confirmed(rpc, our_sigs).await {
                        return Some(Outcome::Won(sig));
                    }
                    sleep(Duration::from_millis(SLEEP_MS)).await;
                }
                return Some(Outcome::Lost(b.round_id));
            }
        }
    }
    None
}

/// Return the first of our signatures that has confirmed *successfully*.
async fn first_confirmed(rpc: &RpcClient, our_sigs: &[Signature]) -> Option<Signature> {
    for sig in our_sigs {
        if let Ok(Some(Ok(()))) = rpc.get_signature_status(sig).await {
            return Some(*sig);
        }
    }
    None
}

/// Record the outcome, log it with running stats, and return `Ok`.
fn finish(stats: &ResetStats, round_id: u64, outcome: Outcome) -> Result<()> {
    match outcome {
        Outcome::Won(sig) => {
            stats.record_win();
            println!(
                "[reset] round {round_id} WON ✓ ({sig}). [stats] {}",
                stats.summary()
            );
        }
        Outcome::Lost(new_round) => {
            stats.record_loss();
            println!(
                "[reset] round {round_id} LOST — beaten to it (board now {new_round}). [stats] {}",
                stats.summary()
            );
        }
    }
    Ok(())
}

/// Block until the chain reaches `target_slot`.
///
/// Returns early (Ok) if the gap is implausibly large (`> MAX_WAIT_SLOTS`),
/// which happens when `end_slot`/target is effectively unbounded — the caller
/// then re-reads the board instead of sleeping forever.
async fn wait_until_slot(rpc: &RpcClient, target_slot: u64, label: &str) -> Result<()> {
    loop {
        let current = rpc.get_slot().await?;
        if current >= target_slot {
            return Ok(());
        }
        let diff = target_slot - current;
        if diff > MAX_WAIT_SLOTS {
            println!("[{label}] slot gap {diff} > {MAX_WAIT_SLOTS}; abandoning wait");
            return Ok(());
        }
        // Poll coarsely when far out, then tighten on the final approach so we
        // fire within ~50ms of the target rather than up to a full tick late.
        let poll_ms = if diff <= 3 { 50 } else { SLEEP_MS };
        sleep(Duration::from_millis(poll_ms)).await;
    }
}
