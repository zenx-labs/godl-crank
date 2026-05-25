//! Transaction submission over a plain RPC endpoint (no Helius Sender / Jito).
//!
//! Two entry points:
//!   * [`submit_confirm`] — one transaction, send + confirm, used for the reset.
//!   * [`send_instructions_bisecting`] — many independent instructions sent in
//!     parallel batches, where a failing batch is split on retry so one bad
//!     instruction can't sink its neighbours.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
#[allow(deprecated)]
use solana_client::send_and_confirm_transactions_in_parallel::{
    send_and_confirm_transactions_in_parallel, SendAndConfirmConfig,
};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::Message,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};
use tokio::time::{sleep, Duration};

/// Solana's hard cap on compute units per transaction.
const MAX_CU_PER_TX: u32 = 1_400_000;

/// Prepend the priority-fee (price) and compute-unit-limit instructions.
fn with_compute_budget(ixs: &[Instruction], priority_fee: u64, cu_limit: u32) -> Vec<Instruction> {
    let mut out = Vec::with_capacity(ixs.len() + 2);
    out.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
    out.push(ComputeBudgetInstruction::set_compute_unit_price(
        priority_fee,
    ));
    out.extend_from_slice(ixs);
    out
}

/// Fire a transaction without waiting for confirmation, skipping preflight.
///
/// Used for the contested reset: we want our transaction on the wire as fast as
/// possible and then poll for the outcome ourselves. The blockhash is supplied
/// (pre-fetched during prep) so firing is a single `sendTransaction` with no
/// round-trip in the critical path. Returns the signature.
pub async fn send_fire(
    rpc: &RpcClient,
    payer: &Keypair,
    ixs: &[Instruction],
    priority_fee: u64,
    cu_limit: u32,
    blockhash: solana_sdk::hash::Hash,
) -> Result<Signature> {
    let all = with_compute_budget(ixs, priority_fee, cu_limit);
    let tx = Transaction::new_signed_with_payer(&all, Some(&payer.pubkey()), &[payer], blockhash);
    let sig = rpc
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow!("send failed: {e}"))?;
    Ok(sig)
}

/// Result of a bisecting batch send. `landed`/`dropped` totals are logged
/// inside the sender; callers act on `dropped` to surface stuck instructions.
pub struct BatchOutcome {
    /// Instructions permanently dropped after isolating them at batch size 1.
    pub dropped: usize,
}

/// Send many *independent* instructions, batched, in parallel.
///
/// A transaction is atomic, so one failing instruction fails its whole batch.
/// To avoid letting a single bad instruction (e.g. a round already closed by
/// someone else) waste the others, every failed batch is re-tried at half the
/// batch size, down to 1. Instructions still failing alone — after a few
/// retries — are dropped and reported, never retried forever.
///
/// `cu_per_ix` sizes each batch's compute-unit limit; `initial_batch_size` is
/// the starting batch width.
// The parallel sender is marked deprecated upstream in favour of a `_v2`
// variant, but the v1 API is pinned via Cargo.lock and works fine here.
#[allow(deprecated)]
pub async fn send_instructions_bisecting(
    rpc: &RpcClient,
    payer: &Keypair,
    label: &str,
    instructions: Vec<Instruction>,
    priority_fee: u64,
    cu_per_ix: u32,
    initial_batch_size: usize,
) -> Result<BatchOutcome> {
    let parallel_rpc = Arc::new(RpcClient::new_with_commitment(rpc.url(), rpc.commitment()));
    let total = instructions.len();
    let mut pending = instructions;
    let mut batch_size = initial_batch_size.max(1);
    let mut landed = 0usize;
    let mut dropped = 0usize;
    // Number of full retry passes already spent at batch_size == 1.
    let mut single_attempts = 0u32;
    const MAX_SINGLE_ATTEMPTS: u32 = 3;

    while !pending.is_empty() {
        let chunks: Vec<Vec<Instruction>> =
            pending.chunks(batch_size).map(|c| c.to_vec()).collect();

        let messages: Vec<Message> = chunks
            .iter()
            .map(|ixs| {
                let cu = ((cu_per_ix as u64 * ixs.len() as u64) + 10_000).min(MAX_CU_PER_TX as u64)
                    as u32;
                let full = with_compute_budget(ixs, priority_fee, cu);
                Message::new(&full, Some(&payer.pubkey()))
            })
            .collect();

        println!(
            "[{label}] sending {} tx(s) of up to {batch_size} ix each ({} ix pending)",
            messages.len(),
            pending.len()
        );

        let signers: [&Keypair; 1] = [payer];
        let config = SendAndConfirmConfig {
            with_spinner: false,
            resign_txs_count: Some(3),
        };
        let results = send_and_confirm_transactions_in_parallel(
            parallel_rpc.clone(),
            None,
            &messages,
            &signers,
            config,
        )
        .await
        .map_err(|e| anyhow!("[{label}] parallel send failed: {e}"))?;

        let mut next_pending: Vec<Instruction> = Vec::new();
        for (chunk, res) in chunks.into_iter().zip(results) {
            match res {
                None => landed += chunk.len(),
                Some(err) => {
                    if batch_size == 1 {
                        eprintln!("[{label}] tx failed (will retry/drop): {err:?}");
                    }
                    next_pending.extend(chunk);
                }
            }
        }
        pending = next_pending;

        if pending.is_empty() {
            break;
        }

        if batch_size > 1 {
            // Shrink to isolate the offending instruction(s).
            batch_size = (batch_size / 2).max(1);
        } else {
            // Already isolated to single instructions. Give transient failures
            // a few more shots, then drop them for good.
            single_attempts += 1;
            if single_attempts >= MAX_SINGLE_ATTEMPTS {
                dropped += pending.len();
                eprintln!(
                    "[{label}] dropping {} instruction(s) that keep failing alone",
                    pending.len()
                );
                pending.clear();
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    println!("[{label}] done: {landed}/{total} landed, {dropped} dropped");
    Ok(BatchOutcome { dropped })
}
