# godl-crank

A crank script for the [GODL](https://github.com/zenx-labs/godl-program) Solana
mining protocol. Each round (~1 minute) it submits one atomic
`sample + reveal + reset_permissionless` transaction to advance the board to the
next round. The on-chain program pays the cranker **0.001 SOL** (carved out of
the admin fee) for every reset that lands — that's the incentive to run this.

It also runs a periodic maintenance pass that **checkpoints** eligible miners
and **closes** expired rounds whose rent it owns (reclaiming that rent). Both
loops run concurrently and are individually error-isolated: a transient RPC or
entropy failure logs and retries; it never takes the process down.

Everything runs over a single, plain RPC endpoint — no Helius Sender, no Jito.

## Quick start

```bash
# 1. Put your crank keypair somewhere (e.g. ./wallets/crank.json)
# 2. Configure via .env or flags:
cp .env.example .env      # then edit RPC + CRANK_KEYPAIR

# 3. Run (installs Rust if missing, builds, then runs):
./run.sh
```

Or pass everything on the command line:

```bash
./run.sh \
  --rpc https://your-rpc-provider \
  --keypair ./wallets/crank.json \
  --priority-fee 10000 \
  --close-interval 1
```

`run.sh` forwards all arguments to the binary; `./run.sh --help` lists them.

## Configuration

Every flag has an environment-variable equivalent (so `.env` works) and a
sensible default.

| Flag | Env | Default | Meaning |
|------|-----|---------|---------|
| `--rpc` | `RPC` | _(required)_ | RPC endpoint for reads **and** sends. |
| `--keypair` | `CRANK_KEYPAIR` | _(required)_ | Path to the payer keypair JSON. |
| `--priority-fee` | `PRIORITY_FEE` | `10000` | Priority fee, µLamports per compute unit, applied to every tx. |
| `--compute-unit-limit` | `COMPUTE_UNIT_LIMIT` | `350000` | CU limit for the reset transaction. |
| `--close-interval` | `CLOSE_INTERVAL` | `1` | Hours between checkpoint+close passes. `0` disables maintenance. |
| `--health-interval` | `HEALTH_INTERVAL` | `60` | Seconds between heartbeat logs. |
| `--entropy-hosts` | `ENTROPY_HOSTS` | built-in list | Comma-separated entropy hosts, primary first. |

### Entropy fallback

The seed for each round is fetched from `https://{host}/var/{var}/seed`. Hosts
are tried in order until one returns a valid seed:

```
entropy.godl.dev  →  entropy-1.godl.dev  →  entropy-2.godl.dev  →  entropy-3.vercel.app
```

A dead or slow host (4s timeout) fails over to the next automatically.

## How the maintenance batching works

Checkpoint and close each produce many independent instructions. They are sent
in **parallel batches** (6 instructions per transaction to start). Because a
transaction is atomic, one bad instruction (e.g. a round already closed by
someone else) would fail its whole batch — so any failed batch is retried at
**half the batch size**, down to 1. Instructions that still fail alone after a
few attempts are dropped and reported, never retried forever. This isolates and
discards the bad instructions while landing all the good ones.

## Logs / health

- `[setup]` — startup banner: RPC, payer, balance, board, entropy hosts.
- `[health]` — heartbeat every `--health-interval`s: slot, round, end_slot,
  payer balance (warns under 0.05 SOL).
- `[reset]` — per-round lifecycle: target slot, seed source, top miner,
  submit attempts, confirmation.
- `[checkpoint]` / `[close]` — maintenance pass results (`landed`/`dropped`).
- `[entropy]` — host failover notices.

## Notes

- Pinned to Rust `1.95.0` and a committed `Cargo.lock` for reproducible builds.
- `godl-api` and `entropy-api` are pulled as git dependencies pinned to specific
  commits (see `Cargo.toml`).
- The payer needs enough SOL for fees + per-round priority fees + round rent on
  resets (rent is reclaimed later by `close`). Keep it funded; the heartbeat
  warns when it runs low.
