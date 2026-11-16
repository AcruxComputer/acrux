# acrux-node

New Constellation on Alpenglow: every 50 ms slot, three co-located proposers each stream an encrypted block, exchange signed commitments within 2δlocal, then release their keys in attestations; validators agree on the set P, decrypt, merge by priority across four tracks, execute on the SVM and vote once per four-slot cblock.

This workspace is the node, Acrux Net (twelve validators in FRA, NYC and TYO), the Fault Lab runner and the `acrux` CLI.

## Crates

| Crate | Concern |
|---|---|
| `acrux-crypto` | SHA-256, Ed25519, ChaCha20-Poly1305 slices with H(K) bound in the commitment, Reed-Solomon shreds (64, any 32 rebuild) with Merkle proofs |
| `acrux-consensus` | Votor on cblocks: notarization (60%), notar-fallback, skip, fast finalization (80%), finalization (60%), certificates, ParentReady, safe-to-notar and safe-to-skip |
| `acrux-schedule` | Algorithm 3: proposer-stake caps, α = 1/2, regional runs, systematic sampling, an epoch-seeded ChaCha20 draw |
| `acrux-execution` | Algorithm 4 (four-track selection), execution on LiteSVM with base, priority and inclusion fees and the fee-payer reserve, the lattice state hash, recent blockhashes, threshold transactions (fragments, Shamir key shares, expiry) |
| `acrux-transport` | One QUIC endpoint per validator, each link delayed by its hub geography, Rotor relay sampling by stake, phase profiling |
| `acrux-node` | Algorithm 1 (proposer, hash exchange, attestation) and Algorithm 2 (validity, Agreed, TryExecute, TryVote, timeouts), genesis, the store, the gateway (JSON-RPC, event stream, metrics), rolling facts, the Fault Lab, the CLI |

`../spec/map.json` maps every algorithm line, validity and agreement condition, theorem and lemma to its file, function and Fault Lab scenario.

## Build

```bash
cargo build --release -p acrux-node
cargo test --release --workspace
```

OpenSSL is vendored and built from source on the first build, which needs a C compiler and Perl. `../deploy/Dockerfile` builds the same binary for the host images.

## Run

From `backend/node/` (the default genesis path, `../chain/genesis/acrux-net.json`, is relative to it):

```bash
./target/release/acrux net up
```

Twelve validators, four per hub, three proposers per window drawn by Algorithm 3, link delays from the cities' distances at two thirds of light speed in fibre plus 8 ms, 50 generated transfers a second. The genesis carries the demo pool program, its AXA and AXB mints and vaults, and the faucet. Each start is a fresh genesis.

| Flag | Default | |
|---|---|---|
| `--gateway-port` / `--ws-port` | 4930 / 4931 | JSON-RPC and event stream |
| `--metrics-port` | 4989 | Prometheus `/metrics`, `/health`, and the faucet's grant route `/internal/faucet` (private) |
| `--base-port` | 4950 | first of the twelve node-to-node QUIC ports |
| `--traffic-rate` | 50 | generated transfers a second |
| `--crash-window`, `--crash-proposer` | | one proposer sends nothing for a whole window |
| `--run-seconds` | | stop after this long and write `last-run.json` into the store |
| `--store` | `target/runs/net` | run store; keeps the last `ACRUX_RETAIN_SLOTS` slots (24,000) |
| `--program ADDRESS=PATH` | | another program at genesis |

Environment (`.env.example`): `ACRUX_BCU` (2,000,000 CU per track), `ACRUX_INCLUSION_FEE` (1,000 lamports), `ACRUX_BASE_FEE` (5,000 lamports per signature, half burned), `ACRUX_RESERVE` (1,000,000 lamports), `ACRUX_FRAGMENT_EXPIRY` (8 slots), `ACRUX_TRAFFIC_RATE`, `ACRUX_RETAIN_SLOTS`, `ACRUX_BIND` (127.0.0.1; 0.0.0.0 in a container), `ACRUX_CORS_ORIGINS`.

The node runs at high process priority with raised thread priority, so other work on the machine does not delay a commitment past 2δlocal.

## Gateway

JSON-RPC on `--gateway-port`: `getSnapshot`, `getSlot`, `getRecentSlots`, `getCurrentWindow`, `getSchedule`, `getCounters`, `getFacts`, `getTransaction`, `getLatestBlockhash`, `getBalance`, `getDemoPool`, `getQuote`, `submitTransaction`, `submitFragment`; for followers, `getExecutionSnapshot` (the state at the latest finalized cblock) and `getSlotBlocks` (a slot's encrypted blocks and attestations, for the last 64 slots). The event stream on `--ws-port`. Shapes are `@acrux/contract`'s; the event catalogue is in [../README.md](../README.md).

- `getLatestBlockhash` returns a blockhash from eight slots back, so every validator holds it; a transaction may reference any blockhash of the last 1,200 slots (one minute), and its id is kept for replay protection as long as its blockhash is valid.
- `getFacts` is the last ten minutes of the net, measured on the node clock: pacing within windows, handover per hub link with that link's floor, commit spread, attestation arrival relative to the block, cblock finalization.
- A threshold transaction's fragments, key shares and header signature are byte for byte what `acrux-execution/src/threshold.rs` decodes; `@acrux/sdk` and `seal_for_receivers` produce them.

## Fault Lab

```bash
./target/release/acrux lab run --scenario last-look --seed acrux-last-look-1 --base-port 5016 --out <dir>
```

A sandbox of twelve validators runs for 12 s with the scenario's fault in window 4 and writes `events.jsonl` (every protocol event of windows 3 to 5, appended live) and `outcome.json` (status, outcome, evidence). The API runs it per lab run and keeps both in Postgres.

| Scenario | Fault | Evidence | Lemma |
|---|---|---|---|
| `last-look` | proposer 3 commits 28 ms late | `late_block_dropped`, `skip_certificate` | Lemma 2.3 |
| `censorship` | proposers 1 and 2 leave a transaction out | `transaction_included` (carried by 3, finalized) | Lemma 2.2 |
| `crash` | proposer 3 sends nothing for the window | `two_proposers_continue` (P = {1,2}) | Theorem 2.1 |
| `equivocation` | proposer 3 signs two commitments for one slot | `slashing_proof` (both signatures), `skip_certificate` | Lemma 2.4 |
| `split-commitments` | proposer 3's commitment reaches only proposer 1 | `skip_certificate` | Lemma 2.2 |
| `private-front-run` | receiver 1 holds one fragment of a 2-of-3 transaction | `unreadable_fragment` (1 of 2), then the transaction executes | Section 3.2, Lemma 2.3 |

## CLI

| Command | |
|---|---|
| `acrux net up` / `acrux cluster up --hubs FRA,NYC,TYO` | a local Acrux net |
| `acrux keygen --out key.json` | a Solana keypair file; prints the address |
| `acrux send --keypair key.json --to <address> --lamports N [--n 3 --t 2]` | a plain or t-of-n transfer, followed to finality |
| `acrux node follow <rpc-url>` | follows a net from its latest finalized cblock: loads that cblock's state and checks its hash, then for every finalized slot checks each attestation's signature and Agreed(s), decrypts the agreed blocks with the released keys, checks them against the attested commitments, re-executes them on its own SVM and compares the state hash with the net's and with the finalization certificate |
| `acrux hub register <CODE> --latitude <deg> --longitude <deg>` | `register_hub` on the Hub Registry |
| `acrux hub join <CODE>`, `leave <CODE>`, `move <FROM> <TO>`, `release <CODE>` | the membership instructions and the permissionless release |
| `acrux schedule` | Algorithm 3 for hubs given as JSON on stdin (the API's schedule preview) |
| `acrux lab run` | one Fault Lab scenario |

Hub commands take `--identity key.json` (the validator identity that signs and pays), `--rpc` (default the local validator, http://127.0.0.1:4990) and `--program` (default `ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm`).

## Measured

Acrux Net, 600 s fault-free at 50 transfers a second, twelve validators on one machine (`node scripts/summarize-run.mjs <store>` reads a run's `last-run.json`):

| | |
|---|---|
| Skipped cblocks | 0 |
| Finalized cblocks | 2,640, every cblock of the run |
| State hashes | 10,564 checks across the twelve validators, 0 mismatches |
| Execution skew between validators | at most 2 slots |
| Slot pacing within a window | p50 50.0 ms, p99 51.3 ms |
| Commit spread | p50 4.7 ms, p99 12.0 ms |
| Attestation arrival after the block | p50 48.5 ms, p99 76.1 ms |
| Cblock finalization | p50 324 ms, p99 398 ms |

Window handovers per hub link. The floor is 50 ms plus the arrival of the second hub's votes for the 60% certificate, since Rotor's relays sit in every hub:

| Link | p50 | p99 | Floor |
|---|---:|---:|---:|
| FRA-FRA | 149.1 ms | 156.1 ms | 134.9 ms |
| NYC-NYC | 149.3 ms | 156.0 ms | 134.9 ms |
| TYO-TYO | 180.6 ms | 189.0 ms | 166.7 ms |
| FRA-NYC | 184.3 ms | 187.6 ms | 169.9 ms |
| NYC-TYO | 163.6 ms | 163.7 ms | 150.8 ms |
| TYO-FRA | 170.2 ms | 171.9 ms | 158.4 ms |

What a send costs on Acrux Net (10 of each, the live net, a 0.001 SOL transfer with no priority fee; `../sdk/test/cost.mjs`):

| Send | Fee | Submit to executed (median) | Submit to finalized (median) |
|---|---:|---:|---:|
| Plain, to 1 proposer | 6,000 lamports | 339 ms | 580 ms |
| Plain, to all 3 proposers | 8,000 lamports | 342 ms | 585 ms |
| 2-of-3 threshold | 8,000 lamports | 339 ms | 585 ms |

The fee is the 5,000-lamport base fee plus 1,000 lamports for each included copy or fragment. A 2-of-3 send costs what a plain send to the same three proposers does, 2,000 lamports more than a plain send to one, and arrives just as fast.

One proposer down for a whole window (`--crash-window`): 0 skips over 120 s, the window continues on P = {1, 2}.
