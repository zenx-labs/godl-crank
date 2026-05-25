#!/usr/bin/env bash
#
# run.sh — one-command launcher for the GODL crank bot.
#
# Bootstraps the Rust toolchain if it's missing, builds the release binary,
# and runs it. Configuration comes from a .env file (see .env.example) or from
# flags passed straight through to the bot, e.g.:
#
#   ./run.sh --rpc https://my-rpc --keypair ./wallets/crank.json
#   ./run.sh --rpc https://my-rpc --keypair ./wallets/crank.json \
#            --priority-fee 50000 --close-interval 2
#
# Any argument after `run.sh` is forwarded to the `godl-crank` binary.
# Run `./run.sh --help` to see every flag.

set -euo pipefail
cd "$(dirname "$0")"

REQUIRED_RUST="1.95.0"

# --- Ensure cargo is available -------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "[run] Rust/cargo not found — installing rustup (toolchain ${REQUIRED_RUST})..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain "${REQUIRED_RUST}" --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

echo "[run] using $(cargo --version)"

# --- Build ---------------------------------------------------------------------
echo "[run] building release binary (first build pulls Solana deps; be patient)..."
cargo build --release

# --- Run -----------------------------------------------------------------------
echo "[run] launching godl-crank..."
exec ./target/release/godl-crank "$@"
