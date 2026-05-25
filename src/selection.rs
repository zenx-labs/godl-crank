//! Top-miner selection for `reset_permissionless`.
//!
//! The on-chain handler (program/src/miner/reset_permissionless.rs) is strict:
//! for a non-split round with a non-empty winning square it loads the supplied
//! `top_miner` as a `Miner`, asserts it played this round, and verifies the
//! winning sample falls inside its cumulative range on the winning square. If
//! we pass the wrong account (or the default pubkey), the reset reverts.
//!
//! Pool-vs-solo is decided ON-CHAIN from the miner's canonical `PoolMember`
//! PDA — so off-chain we just need to find the *actual* winner (solo or
//! pooled) and hand it over. We must NOT short-circuit to the default pubkey
//! for solo winners (that was the legacy `_strict` bug for this path).
//!
//! The default pubkey is correct ONLY when the program never inspects the
//! candidate: empty winning square, split reward, or no usable RNG.

use anyhow::{Context, Result};
use bincode::deserialize;
use entropy_api::state::Var;
use godl_api::prelude::*;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::{keccak, slot_hashes::SlotHashes};
use solana_sdk::{pubkey::Pubkey, sysvar};

use crate::rpc::get_miners_participating;

pub struct TopMiner {
    pub authority: Pubkey,
    pub winning_square: usize,
    pub sample: u64,
}

/// Decide which miner authority to pass to `reset_permissionless`.
///
/// Returns `Ok(None)` when the default pubkey is the correct value (split,
/// empty winning square, or unusable RNG).
pub async fn select_top_miner(
    rpc: &RpcClient,
    round: &Round,
    var: &Var,
    seed: [u8; 32],
) -> Result<Option<TopMiner>> {
    let slot_hash = resolve_slot_hash(rpc, var).await?;
    let value = derive_var_value(slot_hash, seed, var.samples);
    let rng = derive_round_rng(value);

    let winning_square = round.winning_square(rng);

    // Empty winning square: program vaults everything, never reads top_miner.
    if round.deployed[winning_square] == 0 {
        return Ok(None);
    }
    // Split reward: program sets SPLIT_ADDRESS, never reads top_miner.
    if round.is_split_reward(rng) {
        return Ok(None);
    }

    let sample = round.top_miner_sample(rng, winning_square);

    // Find the miner whose cumulative range on the winning square contains the
    // sample. This is the unique winner the program will verify.
    let miners = get_miners_participating(rpc, round.id).await?;
    for (_, miner) in miners {
        if miner.round_id != round.id {
            continue;
        }
        let deployed = miner.deployed[winning_square];
        if deployed == 0 {
            continue;
        }
        let lower = miner.cumulative[winning_square];
        let Some(upper) = lower.checked_add(deployed) else {
            continue;
        };
        if sample >= lower && sample < upper {
            return Ok(Some(TopMiner {
                authority: miner.authority,
                winning_square,
                sample,
            }));
        }
    }

    // We expected a winner but couldn't find one (RPC lag / index gap). Returning
    // None would make the program revert, so surface it as an error and let the
    // caller retry rather than silently submitting a doomed reset.
    anyhow::bail!(
        "winning square {winning_square} has {} lamports deployed but no miner range contained sample {sample}",
        round.deployed[winning_square]
    )
}

/// Reconstruct the slot hash the entropy program will use.
///
/// If the var already carries one, use it. Otherwise look it up in the
/// SlotHashes sysvar for `var.end_at`, falling back to the program's
/// `keccak(end_at)` default when the slot has aged out of the sysvar.
pub async fn resolve_slot_hash(rpc: &RpcClient, var: &Var) -> Result<[u8; 32]> {
    if var.slot_hash != [0; 32] {
        return Ok(var.slot_hash);
    }

    let account = rpc
        .get_account(&sysvar::slot_hashes::ID)
        .await
        .context("failed to fetch slot hashes sysvar")?;
    let slot_hashes: SlotHashes =
        deserialize(&account.data).context("failed to deserialize slot hashes sysvar")?;
    if let Some((_, hash)) = slot_hashes.iter().find(|(slot, _)| *slot == var.end_at) {
        return Ok(hash.to_bytes());
    }
    Ok(keccak::hashv(&[&var.end_at.to_le_bytes()]).to_bytes())
}

fn derive_var_value(slot_hash: [u8; 32], seed: [u8; 32], samples: u64) -> [u8; 32] {
    keccak::hashv(&[&slot_hash, &seed, &samples.to_le_bytes()]).to_bytes()
}

fn derive_round_rng(value: [u8; 32]) -> u64 {
    let r1 = u64::from_le_bytes(value[0..8].try_into().unwrap());
    let r2 = u64::from_le_bytes(value[8..16].try_into().unwrap());
    let r3 = u64::from_le_bytes(value[16..24].try_into().unwrap());
    let r4 = u64::from_le_bytes(value[24..32].try_into().unwrap());
    r1 ^ r2 ^ r3 ^ r4
}
