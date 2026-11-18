# acrux-node

New Constellation on Alpenglow: every 50 ms slot, three co-located proposers each stream an encrypted block, exchange signed commitments within 2δlocal, then release their keys in attestations; validators agree on the set P, decrypt, merge by priority across four tracks, execute on the SVM and vote once per four-slot cblock.

Acrux is a chain of its own, a faster Solana fork: Solana's transactions and programs on the SVM, ordered by concurrent proposers. Acrux Net is that chain: twelve validators in FRA, NYC and TYO. This workspace is its node, the Fault Lab runner and the `acrux` CLI.

## ACRUX

ACRUX is the chain's native token, with 9 decimals: amounts are in lamports, 1 ACRUX = 1,000,000,000 lamports, the unit every `lamports` field of the RPC, the API, the SDK and the CLI carries. Fees are paid in it (the base fee of 5,000 lamports per signature, half burned; 1,000 lamports per included copy or fragment; the priority fee), the fee payer's reserve is 0.001 ACRUX, and the faucet grants test ACRUX (2 ACRUX a grant, `FAUCET_GRANT_LAMPORTS`).

## Hub Registry on Acrux

The genesis loads the same `hub_registry` program as Solana's (`ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm`) with its accounts: the registry (deposit 1 ACRUX, fee 500 bps, cooldown 1 epoch, inactivity 4 epochs, n_min 4), the hubs FRA, NYC and TYO, and the twelve validators' memberships. The node takes its hubs, their places and their members from those accounts: the link delays between validators and Algorithm 3's proposer schedule both come from the registry. `acrux hub register|join|leave|move|release --rpc <Acrux RPC>` sends the instructions to Acrux, as `--rpc <Solana RPC>` does to Solana mainnet. An epoch on Acrux is 432,000 slots.

## Bridge proofs

`chain/bridge.json` (or `ACRUX_BRIDGE`) enables the Solana bridge from `fromSlot` for `program` and requires a nonempty `chainId` name. Its optional `epochSlots` defaults to 432,000 and must be positive and divisible by four. This is the bridge light client's epoch length, independent of the consensus scheduler's `windows_per_epoch`; local bridge checks explicitly use 1,536 slots. Every enabled cblock's outbox commits the validator set for the next bridge epoch and its withdrawals.

From the first cblock whose initial slot reaches `fromSlot`, consensus votes sign `SHA256(chainId)` followed by their existing bincode vote bytes. Earlier votes, certificates and their stored encodings remain unchanged. A bridge-enabled node refuses an absent, blank, or published development `ACRUX_IDENTITY_SEED`, and requires `ACRUX_BRIDGE_RPC` to name the two deposit RPCs before starting network listeners. The follower continues checking attestation signatures and re-executed state; its existing historical flow does not require archived bridge proofs for ordinary cblocks.

Set `ACRUX_BRIDGE_PROOF_MAX` when enabling the bridge; it has no default. It bounds the number of archived cblocks shared by withdrawal proofs and one representative per bridge epoch, separately from the rolling 4,096-cblock window. Size it for the withdrawal volume and relayer downtime to support. A full archive refuses a new required proof before its state checkpoint can become final; accepted withdrawal proofs are never evicted. A persistent net keeps the archive in `bridge-proofs.redb` beside `state.redb`. Proofs are archived before their state checkpoint and become readable only once that state is durable, so restarts preserve proofs for persisted withdrawals. A finalized descendant also commits its ancestors: validators supply missing Finalize votes for those finalized ancestors, and the node waits for each ancestor's own relayable quorum and archived proof before saving the descendant's state.

`getCblockProof` accepts exactly one selector: `{consensusSlot}` for that cblock in the rolling window or archive, `{epoch}` for a retained representative of that bridge epoch, or `{latest: true}` for the newest finalized proof held. Proofs include `epochSlots`, `chainId`, the hash preimage, encoded outbox entries and the validators' signed votes. A cblock outside both the rolling window and archive answers `NOT_FOUND`.

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

Twelve validators, four per hub, three proposers per window drawn by Algorithm 3, link delays from the cities' distances at two thirds of light speed in fibre plus 8 ms, 50 generated transfers a second. The genesis carries the demo pool program, its AXA and AXB mints and vaults, and the faucet.

**The chain carries on across restarts.** As each cblock finalizes, validator 0 writes the state at it to `state.redb` in the store, only what changed and in one transaction: every account (programs and their data, token accounts, the registry), the executed transaction ids, the recent blockhashes, the fee accounting, the pending fragments and the built-in traffic's keys. A start finds it and continues from that cblock, a crash included: the same genesis hash, slot and cblock numbers going on from where they were, the next window built on that cblock. `--fresh` sets the saved state aside and starts a new chain from genesis. A new build reads the state an earlier one saved; a change to how transactions execute takes effect from a slot it names, never through a restart.

`acrux net import <rpc>` takes a running net's latest finalized state into `--store` for a net that kept none of its own (0.3.7): its accounts from `getExecutionSnapshot`, its programs from `getProgramAccounts` of each loader, all checked against the net's state hash for that cblock before anything is written. Its genesis hash, counters and blockhashes carry over; the new build's rules apply from the next cblock, so accounts left with no lamports are dropped, and the built-in traffic gets new keys funded as at genesis.

| Flag | Default | |
|---|---|---|
| `--gateway-port` / `--ws-port` | 4930 / 4931 | JSON-RPC and event stream |
| `--metrics-port` | 4989 | Prometheus `/metrics`, `/health`, and the faucet's grant route `/internal/faucet` (private) |
| `--base-port` | 4950 | first of the twelve node-to-node QUIC ports |
| `--traffic-rate` | 50 | generated transfers a second |
| `--crash-window`, `--crash-proposer` | | one proposer sends nothing for a whole window |
| `--run-seconds` | | stop after this long and write `last-run.json` into the store |
| `--store` | `target/runs/net` | the chain's saved state, its kept transactions, and the run store of the last `ACRUX_RETAIN_SLOTS` slots (24,000) |
| `--fresh` | | start a new chain from genesis, setting the saved state aside |
| `--program ADDRESS=PATH` | | another program at genesis |

Environment (`.env.example`): `ACRUX_BCU` (2,000,000 CU per track), `ACRUX_INCLUSION_FEE` (1,000 lamports), `ACRUX_BASE_FEE` (5,000 lamports per signature, half burned), `ACRUX_RESERVE` (1,000,000 lamports), `ACRUX_FRAGMENT_EXPIRY` (8 slots), `ACRUX_TRAFFIC_RATE`, `ACRUX_RETAIN_SLOTS`, `ACRUX_KEEP_DAYS` (7) and `ACRUX_KEEP_MAX` (200,000) for the kept transactions, `ACRUX_BIND` (127.0.0.1; 0.0.0.0 in a container), `ACRUX_CORS_ORIGINS`.

The node runs at high process priority with raised thread priority, so other work on the machine does not delay a commitment past 2δlocal.

## Gateway

JSON-RPC on `--gateway-port`: `getSnapshot`, `getSlot`, `getRecentSlots`, `getCurrentWindow`, `getSchedule`, `getCounters`, `getFacts`, `getTransaction`, `getLatestBlockhash`, `getBalance`, `getDemoPool`, `getQuote`, `submitTransaction`, `submitFragment`; for followers, `getExecutionSnapshot` (the state at the latest finalized cblock) `getSlotBlocks` (a slot's encrypted blocks and attestations, for the last 256 slots) and `getSlotRange` (`{from, count}`, up to 64 consecutive slots, each with its view and blocks, so a follower far from the gateway pays one round trip for many slots). The event stream on `--ws-port`. Shapes are `@acrux/contract`'s; the event catalogue is in [../README.md](../README.md).

- A submission targets a slot up to 64 ahead. One that arrives after its target has started (a client far from the gateway, a wallet that took its time to sign) is admitted for 1,024 slots more and goes to the proposers then due, in the same roles. Its fee payer must hold the inclusion fee of every copy it sends, within its per-window rate; the gateway refuses it otherwise.
- A proposer carries only copies whose payer can pay their inclusion fee and whose blockhash is still valid. A validator charges a slot's copies in block order and drops any its payer can no longer pay, so a submitted transaction never makes an honest block invalid. A transaction whose blockhash ages out before it executes fails with no fee. One carried in a slot that was skipped goes out again two windows later, to the proposer then in its role, until it executes or its blockhash expires.
- `getLatestBlockhash` returns a blockhash from eight slots back, so every validator holds it; a transaction may reference any blockhash of the last 1,200 slots (one minute), and its id is kept for replay protection as long as its blockhash is valid.
- `getFacts` is the last ten minutes of the net, measured on the node clock: pacing within windows, handover per hub link with that link's floor, commit spread, attestation arrival relative to the block, cblock finalization.
- A threshold transaction's fragments, key shares and header signature are byte for byte what `acrux-execution/src/threshold.rs` decodes; `@acrux/sdk` and `seal_for_receivers` produce them.

**Solana's JSON-RPC** on the same endpoint, in Solana's shapes, so the `solana` CLI, web3.js and wallets work against it with only the URL changed: a request with array params is Solana's, and a batch (a JSON array of requests, up to 100) is answered in order. Every browser origin may call it unless `ACRUX_CORS_ORIGINS` names a list (`*` opens it). `ACRUX_LOG_RPC=1` writes one line per Solana call (method and encoding) to stderr, to see what a wallet asks. Methods: `getHealth`, `getVersion`, `getGenesisHash` (the genesis state's hash), `getSlot`, `getBlockHeight`, `getEpochInfo`, `getLatestBlockhash`, `isBlockhashValid`, `getBalance`, `getAccountInfo`, `getMultipleAccounts` (up to 100) and `getProgramAccounts` (`base64`, `base58`, or `jsonParsed`, which parses SPL Token and Token-2022 accounts and mints and gives other accounts as base64), `getTokenAccountsByOwner` (by `mint` or `programId`; both token programs when the filter names neither), `getTokenAccountBalance`, `getTokenSupply`, `getMinimumBalanceForRentExemption`, `getFeeForMessage` (base fee, priority fee and three inclusion fees, as `sendTransaction` sends), `getRecentPrioritizationFees`, `getVoteAccounts`, `simulateTransaction` (`sigVerify`, `replaceRecentBlockhash`), `sendTransaction` (base58 or base64; simulated first unless `skipPreflight`, then sent to the next slots' three proposers; a resend of the same signed transaction answers its signature and, until it executes, goes out again), `getSignatureStatuses` (up to 256 signatures; `processed` once executed, `finalized` once its cblock is: an executed transaction can still go with a skipped cblock, so it is never reported `confirmed` before it is final), `getTransaction` (`json`, `jsonParsed`, `base64` or `base58`; `finalized` by default, `confirmed` on request; `blockTime` is when the slot executed; `preBalances`, `postBalances`, `preTokenBalances` and `postTokenBalances` for the message's accounts; `jsonParsed` parses System, SPL Token, Token-2022, Associated Token Account and Memo instructions and gives the rest, Compute Budget included as on Solana, with program, accounts and data), `getBlockTime` and `getSignaturesForAddress` (newest first, `limit`, `before`, `until`). Acrux's own `getTransaction` also takes `{signature}` in place of `{id}`.

**Kept transactions.** A transaction anyone sends, the built-in traffic and the faucet's credits aside, is kept in `history.redb` once its cblock finalizes: its signature, slot, accounts, logs, fee, balances, result and finalization, with its Acrux record. `getTransaction` in both shapes, `getSignatureStatuses` and `getSignaturesForAddress` answer from it once the gateway's own records of the last 20 minutes have moved on, across restarts. It keeps `ACRUX_KEEP_DAYS` (7) days and at most `ACRUX_KEEP_MAX` (200,000) transactions, the oldest going first; each takes about 6.8 KB, so the most it holds is about 1.4 GB. A slot's own detail stays for the 20 minutes, and an older one is answered as such. `getOutsideUse` (`{hours}`, 24 by default, up to 168) gives the transactions kept and their distinct fee payers for each UTC hour, and `/metrics` the current hour's as `acrux_outside_transactions_hour` and `acrux_outside_fee_payers_hour`. Programs come with the genesis (`--program ADDRESS=PATH`, or the genesis file's `programs`).

A validator that falls more than two windows behind catches up: it executes and votes the windows after its last executed slot, oldest first, eight a tick, producing nothing for them. The gateway runs on threads of its own, apart from the validators. Each event is serialized once for every viewer; a viewer that falls behind misses events and gets a fresh snapshot at most every ten seconds; `ACRUX_WS_MAX` (200) caps stream connections in all, beside `ACRUX_WS_PER_IP` (8); a connection over either opens and is closed at once with code 1013 and the reason `RATE_LIMITED`.

**Solana's PubSub** on the JSON-RPC port's own WebSocket upgrade, Solana's alone, so web3.js and wallets find it from the RPC URL (`wss://rpc.<domain>`), and on the event stream's socket (one port up, where web3.js looks for a local node), where a message carrying `jsonrpc` is Solana's beside Acrux's stream commands. A connection that never subscribed to Acrux's stream gets none of its snapshots. `signatureSubscribe` (one notification once the transaction reaches the commitment: `processed` once executed, `confirmed` and `finalized` once its cblock is final), `accountSubscribe` (after each executed slot in which the account changed), `slotSubscribe` (each slot as it starts), and their `…Unsubscribe`; up to 64 subscriptions a connection. Notifications such as web3.js's heartbeat `ping` get no answer, and a client's close is answered, so web3.js closes an idle socket and its process exits. `sendAndConfirmTransaction` confirms through them as it does on Solana.

**Programs.** Any program deploys through the upgradeable loader with the stock CLI: `solana program deploy --use-rpc <program.so>` (`--use-rpc` sends the buffer writes through `sendTransaction`; there is no TPU port). A transaction may call any program; one that calls an account that is not a program fails at execution and pays its fees, as on Solana. The SVM runs every feature but loader v4 and the end of new loader v3 deployments, neither of them on Solana mainnet. A transaction the merge does not place in its slot (its track is full, as when a deploy's buffer writes all lock the buffer) goes back to its proposer and lands in a later slot; it is never recorded as failed. An account a transaction leaves with no lamports no longer exists, as on Solana, so its address can be created again (a deploy closes its buffer so).

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
| `acrux net up` / `acrux cluster up --hubs FRA,NYC,TYO` | a local Acrux net (`--genesis` names the genesis file) |
| `acrux keygen --out key.json` | a Solana keypair file; prints the address |
| `acrux send --keypair key.json --to <address> --lamports N [--n 3 --t 2]` | a plain or t-of-n transfer, followed to finality |
| `acrux node follow <rpc-url>` | follows a net from its latest finalized cblock: loads that cblock's state and checks its hash, then for every finalized slot checks each attestation's signature and Agreed(s), decrypts the agreed blocks with the released keys, checks them against the attested commitments, re-executes them on its own SVM and compares the state hash with the net's and with the finalization certificate |
| `acrux replay --rpc <solana-rpc> --slot <n>` or `--block <file>` | one Solana mainnet block (transaction versions legacy, 0 and 1) through Acrux's merge and Algorithm 4: the leader's order beside the merged order, each transaction's Acrux slot, track and start, how many Acrux slots the block takes, and the sandwiches in it (a trader's buy, a victim's buy of the same asset on the same pool, the trader's sale of the same amount at a profit), with what the trader kept and each sandwich repriced for Acrux: sent 2-of-3 the front leg has nothing to read, so the victim's output is recomputed with it taken out of the pool (a Pump.fun curve's virtual reserves from its trade events; a concentrated-liquidity pool's liquidity and price from its swap events, Meteora DAMM v2, Orca Whirlpool and Raydium CLMM; else the pool vaults on the constant-product rule where it reproduces every trade the block made there), and sent plain the trader's take less the cost of outbidding the victim's priority fee; `--json <file>` writes all of it as one record, `--save <file>` keeps the block to replay again |
| `acrux hub register <CODE> --latitude <deg> --longitude <deg>` | `register_hub` on the Hub Registry |
| `acrux hub join <CODE>`, `leave <CODE>`, `move <FROM> <TO>`, `release <CODE>` | the membership instructions and the permissionless release |
| `acrux schedule` | Algorithm 3 for hubs given as JSON on stdin (the API's schedule preview) |
| `acrux lab run` | one Fault Lab scenario |

Hub commands take `--identity key.json` (the validator identity that signs and pays), `--rpc` (an Acrux RPC such as https://rpc.acrux.computer for Acrux's registry, a Solana RPC for mainnet's; default the local Solana validator, http://127.0.0.1:4990) and `--program` (default `ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm`, the same on both chains).

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

What a send costs on Acrux Net (10 of each, the live net, a 0.001 ACRUX transfer with no priority fee; `../sdk/test/cost.mjs`):

| Send | Fee | Submit to executed (median) | Submit to finalized (median) |
|---|---:|---:|---:|
| Plain, to 1 proposer | 6,000 lamports | 339 ms | 580 ms |
| Plain, to all 3 proposers | 8,000 lamports | 342 ms | 585 ms |
| 2-of-3 threshold | 8,000 lamports | 339 ms | 585 ms |

The fee is the 5,000-lamport base fee plus 1,000 lamports for each included copy or fragment. A 2-of-3 send costs what a plain send to the same three proposers does, 2,000 lamports more than a plain send to one, and arrives just as fast.

One proposer down for a whole window (`--crash-window`): 0 skips over 120 s, the window continues on P = {1, 2}.
