//! Entropy seed fetching with multi-host fallback.
//!
//! The seed for a given `Var` is served at `https://{host}/var/{var}/seed`.
//! We try the primary host first, then each fallback in order, returning the
//! first valid response. A single slow/broken host can never stall the crank.

use std::time::Duration;

use anyhow::{anyhow, Result};
use entropy_types::response::GetSeedResponse;
use solana_sdk::pubkey::Pubkey;

/// Default host list, primary first. Override via `--entropy-hosts`.
pub const DEFAULT_HOSTS: &[&str] = &[
    "entropy.godl.dev",
    "entropy-1.godl.dev",
    "entropy-2.godl.dev",
    "entropy-3.vercel.app",
];

/// Per-host request timeout. Kept short so a dead host fails over quickly; the
/// crank has a ~10s prep window before it must submit.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

pub struct EntropyClient {
    http: reqwest::Client,
    hosts: Vec<String>,
}

impl EntropyClient {
    pub fn new(hosts: Vec<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self { http, hosts }
    }

    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// Fetch the seed for `var`, trying each host until one succeeds.
    ///
    /// Returns the seed and the host that served it (for logging). Errors only
    /// if *every* host fails.
    pub async fn get_seed(&self, var: Pubkey) -> Result<([u8; 32], String)> {
        let mut last_err: Option<String> = None;

        for host in &self.hosts {
            let url = format!("https://{host}/var/{var}/seed");
            match self.try_host(&url).await {
                Ok(seed) if seed != [0u8; 32] => return Ok((seed, host.clone())),
                Ok(_) => {
                    last_err = Some(format!("{host}: returned all-zero seed (not yet revealed)"));
                }
                Err(e) => {
                    last_err = Some(format!("{host}: {e}"));
                }
            }
            // Try the next host on any failure.
            if let Some(err) = &last_err {
                eprintln!("[entropy] host failed, falling back: {err}");
            }
        }

        Err(anyhow!(
            "all entropy hosts failed for var {var}: {}",
            last_err.unwrap_or_else(|| "no hosts configured".into())
        ))
    }

    async fn try_host(&self, url: &str) -> Result<[u8; 32]> {
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("HTTP {}", resp.status()));
        }
        let parsed: GetSeedResponse = resp.json().await?;
        Ok(parsed.seed)
    }
}
