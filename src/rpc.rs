//! Thin async RPC helpers over the GODL program accounts.
//!
//! All reads go through the single user-provided RPC endpoint. Helpers return
//! owned, copied state structs so callers don't hold borrows across awaits.

use anyhow::{anyhow, Result};
use entropy_api::state::Var;
use godl_api::prelude::*;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::pubkey::Pubkey;
use steel::{AccountDeserialize, Clock, Discriminator};

/// Fetch every program account of type `T`, optionally narrowed by `filters`.
///
/// Prepends the account discriminator filter so only `T` accounts come back.
pub async fn get_program_accounts<T>(
    client: &RpcClient,
    filters: Vec<RpcFilterType>,
) -> Result<Vec<(Pubkey, T)>>
where
    T: AccountDeserialize + Discriminator + Clone,
{
    let mut all_filters = vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
        0,
        &T::discriminator().to_le_bytes(),
    ))];
    all_filters.extend(filters);

    let accounts = client
        .get_program_accounts_with_config(
            &godl_api::ID,
            RpcProgramAccountsConfig {
                filters: Some(all_filters),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow!("getProgramAccounts failed (does your RPC support it?): {e}"))?;

    Ok(accounts
        .into_iter()
        .filter_map(|(pubkey, account)| {
            T::try_from_bytes(&account.data)
                .ok()
                .map(|a| (pubkey, a.clone()))
        })
        .collect())
}

pub async fn get_board(rpc: &RpcClient) -> Result<Board> {
    let account = rpc.get_account(&godl_api::state::board_pda().0).await?;
    Ok(*Board::try_from_bytes(&account.data)?)
}

pub async fn get_config(rpc: &RpcClient) -> Result<Config> {
    let account = rpc.get_account(&godl_api::state::config_pda().0).await?;
    Ok(*Config::try_from_bytes(&account.data)?)
}

pub async fn get_var(rpc: &RpcClient, address: Pubkey) -> Result<Var> {
    let account = rpc.get_account(&address).await?;
    Ok(*Var::try_from_bytes(&account.data)?)
}

pub async fn get_round(rpc: &RpcClient, id: u64) -> Result<Round> {
    let account = rpc.get_account(&godl_api::state::round_pda(id).0).await?;
    Ok(*Round::try_from_bytes(&account.data)?)
}

pub async fn get_rounds(rpc: &RpcClient) -> Result<Vec<(Pubkey, Round)>> {
    get_program_accounts::<Round>(rpc, vec![]).await
}

pub async fn get_miners(rpc: &RpcClient) -> Result<Vec<(Pubkey, Miner)>> {
    get_program_accounts::<Miner>(rpc, vec![]).await
}

/// Miners whose `round_id` field equals `round_id`.
///
/// `round_id` lives at byte 544 of the account (8-byte discriminator + 536
/// bytes into the `Miner` struct), so we memcmp there.
pub async fn get_miners_participating(
    rpc: &RpcClient,
    round_id: u64,
) -> Result<Vec<(Pubkey, Miner)>> {
    let filter = RpcFilterType::Memcmp(Memcmp::new_base58_encoded(544, &round_id.to_le_bytes()));
    get_program_accounts::<Miner>(rpc, vec![filter]).await
}

pub async fn get_clock(rpc: &RpcClient) -> Result<Clock> {
    let data = rpc.get_account_data(&solana_sdk::sysvar::clock::ID).await?;
    Ok(bincode::deserialize::<Clock>(&data)?)
}
