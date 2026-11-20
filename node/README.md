# acrux-node

New Constellation on Alpenglow: every 50 ms slot, three co-located proposers each stream an encrypted block, exchange signed commitments within 2δlocal, then release their keys in attestations; validators agree on the set P, decrypt, merge by priority across four tracks, execute on the SVM and vote once per four-slot cblock.

Acrux is a chain of its own, a faster Solana fork: Solana's transactions and programs on the SVM, ordered by concurrent proposers. Acrux Net is that chain: twelve validators in FRA, NYC and TYO. This workspace is its node, the Fault Lab runner and the `acrux` CLI.

## ACRUX

Native amounts have 9 decimals: 1 ACRUX = 1,000,000,000 lamports. A bridge rule can name the slot at which bridged ACRUX becomes this native balance, without a fresh genesis. Before that slot the existing test-lamport ledger and SPL bridge continue. At it, system wallets' old lamports are retired, bridged ACRUX balances convert to their token authorities' native accounts, and the faucet and generated traffic stop. Data, token and program accounts retain the cut-over reserve. The fee rates and burn rules remain the existing ones; the later road steps change fees and stake.

## Hub Registry on Acrux

The genesis loads the same `hub_registry` program as Solana's (`ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm`) with its accounts: the registry (deposit 1 ACRUX, fee 500 bps, cooldown 1 epoch, inactivity 4 epochs, n_min 4), the hubs FRA, NYC and TYO, and the twelve validators' memberships. The node takes its hubs, their places and their members from those accounts: the link delays between validators and Algorithm 3's proposer schedule both come from the registry. `acrux hub register|join|leave|move|release --rpc <Acrux RPC>` sends the instructions to Acrux, as `--rpc <Solana RPC>` does to Solana mainnet. An epoch on Acrux is 432,000 slots.

## Bridge proofs

`chain/bridge.json` (or `ACRUX_BRIDGE`) enables the Solana bridge from `fromSlot` for `program` and requires a nonempty `chainId` name. Its optional `epochSlots` defaults to 432,000 and must be positive and divisible by four. This is the bridge light client's epoch length, independent of the consensus scheduler's `windows_per_epoch`; local bridge checks explicitly use 1,536 slots. Every enabled cblock's outbox commits the validator set for the next bridge epoch and its withdrawals.

From the first cblock whose initial slot reaches `fromSlot`, consensus votes sign `SHA256(chainId)` followed by their existing bincode vote bytes. Earlier votes, certificates and their stored encodings remain unchanged. A bridge-enabled node refuses an absent, blank, or published development `ACRUX_IDENTITY_SEED`, and requires `ACRUX_BRIDGE_RPC` to name the two deposit RPCs before starting network listeners. The follower continues checking attestation signatures and re-executed state; its existing historical flow does not require archived bridge proofs for ordinary cblocks.

The optional `minWithdrawal` object maps each Solana mint address to its minimum withdrawal amount as a decimal u64 string in base units. The executor applies that rule from `fromSlot`; mints absent from the map have no minimum. Keep the same bridge rule on every validator and follower.

### Native cut-over rule

Configure these three optional bridge-rule fields together. Omitting all three
preserves the SPL bridge behavior:

- `nativeFromSlot`: the operator-selected four-slot cblock boundary, strictly
  after `fromSlot`. It has no default.
- `nativeMint`: the Solana mint becoming native; mainnet ACRUX is
  `7Evw4uxTxLGGEFEvXLUNYyc1J7PnFk9EYwTUFagARUX`.
- `nativeScale`: native lamports per Solana base unit, `1000` for ACRUX's six
  Solana decimals and nine native decimals.

Every validator and follower needs the same rule and append-only placement
manifest before the cut-over. Keep the existing `acrux-bridge.json` placement;
append a separate placement of the new bridge bytecode at `nativeFromSlot`.
The executor checks both the executable account and its ProgramData, so another
placement at the same program address upgrades the code. The host's actual slot
and live rule are operator configuration; this release does not select one.

At the named slot, the executor converts every classic SPL token account of the
bridged mint, including non-associated, frozen and delegated accounts. It zeros
those token balances and the mint supply, retires all empty system wallets, then
credits each token authority with `amount * nativeScale`. Other accounts retain
their data and lamports. A hashed `['native']` bridge PDA records the fixed
reserve, converted/deposited native lamports, withdrawn native lamports and rule.
Its presence makes conversion idempotent across replay, restore and rollback.
Native deposits then credit the recipient's system wallet directly; an invalid
recipient or overflow uses the existing skipped-credit record.

`withdrawNative(amount, recipient)` takes an amount in Solana base units. It
checks the configured minimum and the aggregate credited/withdrawn cap, moves
`amount * nativeScale` to `['native_burn']`, and writes the existing Withdrawal
record. At the end of the slot the executor removes the burn PDA's excess over
rent. The executor alone installs the native gate, so this instruction cannot
run against Solana's bridge. The aggregate cap does not track the provenance
of wallet lamports: unlocked legacy reserve can consume backing otherwise held
by native holders. The fixed reserve is accounting, not a lock on those accounts.

From activation, private `GET`/`POST /internal/faucet` returns HTTP 410 with
`FAUCET_RETIRED`, the fixed message and `cutoverSlot`; public faucet routes carry
the same code and slot in the contract error envelope. Generated traffic stops
without funding replacement senders. Existing native fees still burn their
configured fraction, so circulating plus native fee burns is the relevant
comparison with the Solana vault while releases are settled.

Set `ACRUX_BRIDGE_PROOF_MAX` when enabling the bridge; it has no node default
(host examples use 100,000). It is a soft shedding level, never an insertion cap.
Every withdrawal proof and one representative per bridge epoch are stored above
it too, separately from the rolling 4,096-cblock window. Representatives are
permanent and uncapped. `ACRUX_BRIDGE_PROOF_GRACE_SLOTS` defaults to 1,728,000
slots (24 hours at 50 ms). After durable publication, excess non-representative
withdrawal proofs older than that age are shed oldest first, at most 100 per
dispatch, down toward the level, only when every withdrawal in the proof is
below the keeper's reported saved cursor. With no report, nothing is age-shed. Age follows finalized Acrux slots rather than
wall time. Young proofs and representatives can keep the archive above the level.
`bridge_proof_archive_full` logs once on entering that state; execution and
proof inserts continue.

Bridging requires a persistent node store. The archive in `bridge-proofs.redb`
keeps proof payloads on disk with a 64 MiB redb page cache; node and gateway memory
hold only slot/epoch/withdrawal indexes, beside the 4,096 rolling proofs.
The private port accepts `POST /internal/bridge-saved` with `{program,
nextWithdrawalSequence}` (a decimal u64 string). It checks the program and accepts
only monotonic acknowledgements; reports are volatile, so a restart waits for
the keeper to report its Postgres cursor again. Shedding logs aggregate at
1,024 removed proofs instead of logging every dispatch. The background Solana reader prunes a withdrawal proof only after every withdrawal it contains is paid according to matching finalized release bitmaps from both configured RPCs. It validates each bitmap's owner, discriminator, bucket, length and bytes. One representative per epoch remains for handoffs after its withdrawals are paid. The age-based policy can shed old unpaid non-representatives; the keeper retains its attempted withdrawal proofs separately in Postgres. RPC reads run outside the consensus actor, which applies queued deletions in bounded batches.

Proofs become readable only after their execution state is durable. A finalized descendant also commits its ancestors: validators supply missing Finalize votes for those finalized ancestors, and the node waits for each ancestor's own relayable quorum and prepares its proof before saving the descendant's state. Archive capacity never decides consensus acceptance.

`getCblockProof` accepts exactly one selector: `{consensusSlot}` for that cblock in the rolling window or archive, `{epoch}` for a retained representative of that bridge epoch, or `{latest: true}` for the newest finalized proof held. Proofs include `epochSlots`, `chainId`, the hash preimage, encoded outbox entries and the validators' signed votes. A cblock outside both the rolling window and archive answers `NOT_FOUND`.

## Stake on Acrux

From node 0.4.18, two optional bridge-rule fields enable native ACRUX staking:

- `stakeFromSlot`: an operator-selected Acrux epoch boundary at or after
  `nativeFromSlot`. It has no default. With staking enabled, `epochSlots` must
  be divisible by 16 so a proposer window never straddles a stake epoch.
- `genesisStakeLamports`: a positive unsigned decimal **string**, in native
  lamports, added to each genesis identity's delegated stake. It is required
  with `stakeFromSlot` and has no default. This is virtual proposer weight;
  it creates no spendable balance or staking rewards.

The checked-in placement manifest contains
`{"file":"stake.json","slot":"stakeFromSlot"}`. That entry resolves to the
rule's slot and is disabled while the rule omits it. Numeric placement slots
continue to work. Thus the operator selects the real slot in the rule once,
without changing the current live rule or guessing a slot in this release.
Distribute the rule and matching placement manifest to validators and followers
before activation. The Stake bytecode in `chain/placements/stake.json` is
already fetched; this step does not deploy a Solana program.

At activation the executor installs twelve rent-exempt Vote accounts, one per
genesis identity, at the Vote-program PDA seeds
`["acrux_genesis_vote", identity_bytes]`. Each valid VoteState names that identity
as node, authorized voter and authorized withdrawer. Solana's placed Stake
program reads those accounts; Acrux does not run a Vote program. The protocol's
new Stake placement, Vote and activation-account rent joins the recorded native
reserve, so installing them adds no circulating ACRUX. A hashed activation account
retains three fixed-size schedule snapshots and the Stake program's own minimum
delegation. Restore and rollback reproduce activation and snapshots.

Clock uses the rule's `epochSlots` from the stake slot (432,000 on mainnet).
StakeHistory stays empty: stake becomes fully active in the epoch after
activation, and fully withdrawable in the epoch after deactivation. The live
weight predicate is `activation_epoch < epoch && deactivation_epoch >= epoch`.
There is no reward processing or issuance. The genesis weight is added to each
identity's counted delegations in `getVoteAccounts` and the proposer draw.

The first canonical slot of epoch `e + 1` freezes the state inherited from the
last canonical slot of `e` for the draw of `e + 2`. Delegations are evaluated for
that target epoch. The snapshot becomes usable when its checkpoint finalizes;
no wall-clock or RPC observation changes a draw. The two bootstrap epochs use
only configured genesis weight. Draws use the existing seed and
`windows_per_epoch` (96), with `build_schedule(target_epoch, ...)` and the same
global `window_number % 96` indexing. Earlier slots use the byte-identical legacy
draw. Consensus certificate weights and bridge validator-set weights remain the
existing genesis weights in this proposer-only step.

The lead requires finality during the intervening epoch. Window preparation
waits if its required finalized snapshot is unavailable; it does not substitute
another draw. A whole epoch with no canonical execution can require a restart
from durable finalized state to resume the slot clock. Algorithm 3's existing
hub admission rule also remains: four members and at least 1% of total raw
stake after capping. For the current three four-member hubs, baseline weight
`B` per identity and total delegated stake `D`, `D <= 388 * B` is a sufficient
bound to keep every hub eligible. Select the genesis weight for the intended
maximum delegation; a draw with no eligible hub fails rather than inventing a
fallback schedule.

`getVoteAccounts` returns each identity's real `votePubkey`, `nodePubkey` and
current activated stake plus virtual genesis weight. `getStakeActivation`,
`getStakeMinimumDelegation` and `getEpochInfo` support stock tooling. Acrux's
`getProposerEpoch {epoch}` returns `{epoch, weights: [{identity, stake}], schedule}`
with decimal-string weights and the full Algorithm 3 draw. Existing `getSchedule`
continues serving upcoming proposer windows. Execution snapshots include public
proposer configuration so followers need no validator private seed to rebuild
and check the same draws. The API already reads Acrux `getVoteAccounts`; no API
or contract shape change is required.

Use an existing funded native wallet and a separate stake-account keypair.
Read `getVoteAccounts` to choose the vote address belonging to the intended
identity, then use stock commands (replace `VOTE_ACCOUNT` and `RECIPIENT`):

```text
solana --url https://rpc.acrux.computer --keypair wallet.json create-stake-account stake.json 2
solana --url https://rpc.acrux.computer --keypair wallet.json delegate-stake stake.json VOTE_ACCOUNT
solana --url https://rpc.acrux.computer --keypair wallet.json deactivate-stake stake.json
solana --url https://rpc.acrux.computer --keypair wallet.json withdraw-stake stake.json RECIPIENT ALL
```

Wait until the epoch after deactivation before withdrawing the whole account.
The CLI labels its amount SOL; at an Acrux URL one displayed unit is one ACRUX
(10^9 native lamports). Keep additional wallet balance for fees and rent. With
web3.js use `StakeProgram.createAccount`, `delegate`, `deactivate` and `withdraw`;
fund the stake account with the intended delegation plus its queried rent.

The operator's local check, from `chain/`, is:

```text
node scripts/native-stake-check.mjs --native-from-slot 8192 --stake-from-slot 8448 --epoch-slots 256 --native-scale 1000 --genesis-stake-lamports 1000000000
```

These explicit local values are not production defaults. With an existing fresh
Solana validator on 4990, the rebuilt native bridge and a private local identity
seed, it deposits before native activation, delegates after stake activation,
checks the epoch-two draw and live vote weights, then deactivates and withdraws.
It prints the schedule, stake, weights and zero rewards. Its own net uses TCP
5041-5043 and QUIC 5062-5073, and it removes its child and temporary store.

## Epoch fee burn

The bridge rule may add `feeBurnFromSlot`, with no default: a bridge-epoch
boundary at or after `nativeFromSlot`. From it, hashed state sums actual burned
base fees. The epoch's last cblock appends a positive bincode variant-2
`FeeBurn {epoch, amount}` in Solana mint base units; rounding dust carries. A
skipped ending carries all unallocated fees to the next executed ending. Existing
ValidatorSet/Withdrawal bytes and their SHA-256 Merkle leaves stay unchanged.

`getNativeSupply` adds `feeBurnedLamports`, cumulative native lamports committed
in whole-unit burn entries. Gross credits keep their public meaning. Internally
the separate hashed counter reserves raw burns against the existing native gate,
so no new Acrux bytecode or placement is required. Followers configure the same
rule and re-execute those counter updates and state hashes.

`getCblockProof {feeBurnEpoch}` reads the permanently retained epoch-ending proof,
including zero epochs. It returns null only for an ending skipped behind durable
finality; an incomplete epoch is NOT_FOUND. The keeper hands on, burns on Solana
against that proof, waits for a finalized replay bit, and then cleans up. Keep
the existing Acrux placements: the new `.so` is a Solana-side upgrade. Operator
setup, the exact vault equation and the deferred validator check are in
[../docs/bridge-relayer.md](../docs/bridge-relayer.md#epoch-base-fee-burns).

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

`acrux net import <rpc>` takes a running net's latest finalized state into `--store` for a net that kept none of its own (0.3.7): its accounts from `getExecutionSnapshot`, its programs from `getProgramAccounts` of each loader, all checked against the net's state hash for that cblock before anything is written. Its genesis hash, counters and blockhashes carry over; the new build's rules apply from the next cblock, so accounts left with no lamports are dropped, and the built-in traffic gets new keys funded as at genesis. An already activated native snapshot receives no new traffic funding.

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

JSON-RPC on `--gateway-port`: `getSnapshot`, `getSlot`, `getRecentSlots`, `getCurrentWindow`, `getSchedule`, `getCounters`, `getFacts`, `getTransaction`, `getLatestBlockhash`, `getBalance`, `getDemoPool`, `getQuote`, `submitTransaction`, `submitFragment`. The event stream is on `--ws-port`. Explorer shapes are `@acrux/contract`'s; the event catalogue is in [../README.md](../README.md).

Followers use a separate, smaller read path:

- `getFinalizedCblock {}` returns `{consensusSlot}`, the newest durable finalized
  consensus slot, or `null` in that field before any cblock is available.
- `getExecutionSnapshot {}` returns the executed state and threshold cache at a
  finalized anchor. Its anchor metadata accompanies the same checkpoint, so a
  follower can validate the loaded state without racing a later slot lookup.
- `getFollowerPage {from, count}` returns up to 256 consecutive slots with only
  their replay/checking metadata and encrypted blocks. It includes state hashes,
  agreed sets, window proposer identities, cblock parents and certificate summaries;
  signed attestations appear once with the blocks. The response is
  `{finalized: {consensusSlot}, slots: [{slot, view, blocks}]}`. Missing history
  is represented by unavailable slot data, so the follower can re-anchor.
- `getSlotBlocks {slot}` serves retained encrypted blocks and attestations.
  Gateway node 0 retains 2,400 slots, about two minutes at 50 ms. Metrics expose
  `acrux_follower_retained_slots` and `acrux_follower_retained_block_bytes`; the
  latter counts retained replay buffers, excluding allocator/map overhead.
- `getSlotRange {from, count}` keeps its existing explorer shape and 64-slot limit.

`acrux node follow <url> --genesis <path>` reuses an HTTP connection, polls the
small finalized-head call and re-executes compact pages. It still checks signed
attestations, agreement, decrypted commitments, transactions, certificates and
state hashes. If required blocks have expired, it prints a re-anchor line,
loads a fresh execution snapshot and continues. Verification failures remain
fatal; re-anchoring does not substitute for checking a mismatched state.

The Solana array-parameter surface also serves `getSupply`: a context and value
with `total`, `circulating`, `nonCirculating` and `nonCirculatingAccounts`.
Total is all executor lamports. Before native activation the reserve is zero.
After activation `nonCirculating` is the hashed reserve (the cut-over reserve
plus protocol-installed Stake activation rent), and `circulating` is total minus
that reserve. The account list is empty because the
reserve is a fixed amount, rather than a live classification of addresses.
`getNativeSupply {}` returns `null` before activation; afterwards it returns
`{cutoverSlot, mint, scale, reserveLamports, creditedLamports, withdrawnLamports,
circulatingLamports}`. Amounts and scale are decimal strings; the slot is a
number. The API exposes this as `/v1/bridge`'s `native` field.

**Validator earnings.** `getValidatorEarningsEpochs {}` returns
`{epochSlots, epochs}` with the counted bridge epochs newest first.
`getValidatorEarnings {epoch}` returns `{epoch, countedFromSlot, rows}`. Each row
holds `identity`, its historical `hub`, `validatorKind` (`genesis` or `external`),
and decimal lamport strings `base`, `priority`, and `inclusion`. Members of that
epoch's fixed set with no earnings have zero rows. Base is the unburned part;
priority and inclusion are the amounts actually credited, including each copy
or fragment. Nothing is issued.

Counting starts at the first slot this release executes, with no backfill.
`countedFromSlot > epoch * epochSlots` marks a partly counted epoch. Normal
persistent nets commit the tally and historical roster in `state.redb` with the
same finalized checkpoint as the paid balances, retain every observed epoch,
and restore them on restart. Rollback restores the counters too. These rows
never enter the account-state hash or follower snapshot, so consensus and
followers are unchanged. An unobserved epoch or a non-persistent sandbox answers
`NET_UNAVAILABLE`. The API serves the hub-filtered history at
`GET /v1/hubs/:code/earnings`.

The F7 check is `node node/scripts/follow-far-check.mjs` from `backend/` after a
release build. It checks the usual local net ports before using them, starts its
own net there and a proxy on 5056 with 145 ms delay in each direction, then runs
the real CLI follower for at least five minutes. It checks web3 supply decoding,
compares legacy/compact wire bytes, measures retained buffers and total process
RSS, and verifies re-anchoring after an injected availability gap. It stops only
its own children and proxy and removes its unique test store under `node/target`.

Measured locally on 2026-11-19 with 50 generated transactions/s and a 290 ms
round-trip proxy: 4,656 checked slots over 304.6 seconds, no healthy-run resets,
0-32 slots of sampled lag, then one successful forced re-anchor. The same 64-slot
range used 30,435 bytes/slot in the explorer response and 10,184 bytes/slot in
the follower response (66.5% less); small head replies averaged 55 bytes. The
two-minute retained block allocations ended at 212 MiB, with a highest sampled
value of 226 MiB, excluding map/allocator overhead. Entire 12-validator net RSS
ended at 1.27 GiB;
that is not a gateway-only memory figure. Payloads and memory depend on traffic.


- A submission targets a slot up to 64 ahead. One that arrives after its target has started (a client far from the gateway, a wallet that took its time to sign) is admitted for 1,024 slots more and goes to the proposers then due, in the same roles. Its fee payer must hold the inclusion fee of every copy it sends, within its per-window rate; the gateway refuses it otherwise.
- A proposer carries only copies whose payer can pay their inclusion fee and whose blockhash is still valid. A validator charges a slot's copies in block order and drops any its payer can no longer pay, so a submitted transaction never makes an honest block invalid. A transaction whose blockhash ages out before it executes fails with no fee. One carried in a slot that was skipped goes out again two windows later, to the proposer then in its role, until it executes or its blockhash expires.
- `getLatestBlockhash` returns a blockhash from eight slots back, so every validator holds it; a transaction may reference any blockhash of the last 1,200 slots (one minute), and its id is kept for replay protection as long as its blockhash is valid.
- `getFacts` is the last ten minutes of the net, measured on the node clock: pacing within windows, handover per hub link with that link's floor, commit spread, attestation arrival relative to the block, cblock finalization.
- A threshold transaction's fragments, key shares and header signature are byte for byte what `acrux-execution/src/threshold.rs` decodes; `@acrux/sdk` and `seal_for_receivers` produce them.

**Solana's JSON-RPC** on the same endpoint, in Solana's shapes, so the `solana` CLI, web3.js and wallets work against it with only the URL changed: a request with array params is Solana's, and a batch (a JSON array of requests, up to 100) is answered in order. Every browser origin may call it unless `ACRUX_CORS_ORIGINS` names a list (`*` opens it). `ACRUX_LOG_RPC=1` writes one line per Solana call (method and encoding) to stderr, to see what a wallet asks. Methods: `getHealth`, `getVersion`, `getGenesisHash` (the genesis state's hash), `getSlot`, `getBlockHeight`, `getEpochInfo`, `getLatestBlockhash`, `isBlockhashValid`, `getBalance`, `getSupply`, `getAccountInfo`, `getMultipleAccounts` (up to 100) and `getProgramAccounts` (`base64`, `base58`, or `jsonParsed`, which parses SPL Token and Token-2022 accounts and mints and gives other accounts as base64), `getTokenAccountsByOwner` (by `mint` or `programId`; both token programs when the filter names neither), `getTokenAccountBalance`, `getTokenSupply`, `getMinimumBalanceForRentExemption`, `getFeeForMessage` (base fee, priority fee and three inclusion fees, as `sendTransaction` sends), `getRecentPrioritizationFees`, `getVoteAccounts`, `simulateTransaction` (`sigVerify`, `replaceRecentBlockhash`), `sendTransaction` (base58 or base64; simulated first unless `skipPreflight`, then sent to the next slots' three proposers; a resend of the same signed transaction answers its signature and, until it executes, goes out again), `getSignatureStatuses` (up to 256 signatures; `processed` once executed, `finalized` once its cblock is: an executed transaction can still go with a skipped cblock, so it is never reported `confirmed` before it is final), `getTransaction` (`json`, `jsonParsed`, `base64` or `base58`; `finalized` by default, `confirmed` on request; `blockTime` is when the slot executed; `preBalances`, `postBalances`, `preTokenBalances` and `postTokenBalances` for the message's accounts; `jsonParsed` parses System, SPL Token, Token-2022, Associated Token Account and Memo instructions and gives the rest, Compute Budget included as on Solana, with program, accounts and data), `getBlockTime` and `getSignaturesForAddress` (newest first, `limit`, `before`, `until`). Acrux's own `getTransaction` also takes `{signature}` in place of `{id}`.

**Kept transactions.** A transaction anyone sends, the built-in traffic and the faucet's credits aside, is kept in `history.redb` once its cblock finalizes: its signature, slot, accounts, logs, fee, balances, result and finalization, with its Acrux record. `getTransaction` in both shapes, `getSignatureStatuses` and `getSignaturesForAddress` answer from it once the gateway's own records of the last 20 minutes have moved on, across restarts. It keeps `ACRUX_KEEP_DAYS` (7) days and at most `ACRUX_KEEP_MAX` (200,000) transactions, the oldest going first; each takes about 6.8 KB, so the most it holds is about 1.4 GB. A slot's own detail stays for the 20 minutes, and an older one is answered as such. `getOutsideUse` (`{hours}`, 24 by default, up to 168) gives the transactions kept and their distinct fee payers for each UTC hour, and `/metrics` the current hour's as `acrux_outside_transactions_hour` and `acrux_outside_fee_payers_hour`. Programs come with the genesis (`--program ADDRESS=PATH`, or the genesis file's `programs`).

A validator that falls more than two windows behind catches up: it executes and votes the windows after its last executed slot, oldest first, eight a tick, producing nothing for them. The gateway runs on threads of its own, apart from the validators. Each event is serialized once for every viewer; a viewer that falls behind misses events and gets a fresh snapshot at most every ten seconds; `ACRUX_WS_MAX` (200) caps stream connections in all, beside `ACRUX_WS_PER_IP` (8); a connection over either opens and is closed at once with code 1013 and the reason `RATE_LIMITED`.

**Solana's PubSub** on the JSON-RPC port's own WebSocket upgrade, Solana's alone, so web3.js and wallets find it from the RPC URL (`wss://rpc.<domain>`), and on the event stream's socket (one port up, where web3.js looks for a local node), where a message carrying `jsonrpc` is Solana's beside Acrux's stream commands. A connection that never subscribed to Acrux's stream gets none of its snapshots. `signatureSubscribe` (one notification once the transaction reaches the commitment: `processed` once executed, `confirmed` and `finalized` once its cblock is final), `accountSubscribe` (after each executed slot in which the account changed), `slotSubscribe` (each slot as it starts), and their `…Unsubscribe`; up to 64 subscriptions a connection. Notifications such as web3.js's heartbeat `ping` get no answer, and a client's close is answered, so web3.js closes an idle socket and its process exits. `sendAndConfirmTransaction` confirms through them as it does on Solana.

**Programs.** Any program deploys through the upgradeable loader with the stock CLI: `solana program deploy --use-rpc <program.so>` (`--use-rpc` sends the buffer writes through `sendTransaction`; there is no TPU port). A transaction may call any program; one that calls an account that is not a program fails at execution and pays its fees, as on Solana. The SVM runs every feature but loader v4 and the end of new loader v3 deployments, neither of them on Solana mainnet. A transaction the merge does not place in its slot (its track is full, as when a deploy's buffer writes all lock the buffer) goes back to its proposer and lands in a later slot; it is never recorded as failed. An account a transaction leaves with no lamports no longer exists, as on Solana, so its address can be created again (a deploy closes its buffer so).

## Fault Lab

```bash
./target/release/acrux lab run --scenario last-look --seed acrux-last-look-1 --base-port 5016 --out <dir>
./target/release/acrux lab run --scenario split-commitments --protocol agreement --seed acrux-agreement-1 --base-port 5016 --out <dir>
```

A sandbox of twelve validators runs for 12 s with the scenario's fault in window 4 and writes `events.jsonl` (every protocol event of windows 3 to 5, appended live) and `outcome.json` (protocol, status, outcome, evidence and yardstick). The yardstick keeps node zero's status, votes, certificate kinds and finalization time for every cblock of those three windows. The API runs it per lab run and keeps both in Postgres.

`--protocol today|agreement|lanes` belongs only to `lab run`; it defaults to
`today`. Public
`net up`, RPC followers and outside proposers keep the existing unanimous rule.
There is no activation slot or public-net protocol setting.

`agreement` uses one signed 2-of-3 quorum per block hash. Null entries do not
contradict a hash; conflicting hashes still prevent agreement. The local view
collects authenticated attestations on the control ingress, independently of
validator block-data queues. It closes with all three signed attestations, or two identical vectors whose set a
third non-conflicting row cannot enlarge. Other partial views wait two global
message flights (2 * 250 ms) from the first signed arrival. A delayed local
window timer or a partly drained actor queue cannot close an empty view permanently. Once a quorum exists, its
canonical hashes are fixed before waiting for data and enter the cblock hash
too. A node missing a chosen block or key waits for the existing
Rotor slices and key-release attestations. Validators execute and vote through
the same merge and Votor paths.

`lanes` runs three independent content Votors, one per proposer role. A lane
checks only its proposer's attestation column: at least two signed rows, the
owner's released key, and the same non-null hash in every received row after
all three rows or two global message flights. A split commitment therefore
skip-certifies its own lane; it does not reject the other columns. Votes are
signed with a lane-specific domain and keep the existing 60% and 80% thresholds.

Execution waits for every lane's finalization or skip certificate, then uses the
existing SortTxs, tracks and SVM execution on accepted blocks only. The existing
execution Votor certifies the merged state and advances the window clock. Its
deadline cannot skip a cblock while content decisions are still pending. Lab
cblock preimages commit to the accepted lane hashes in proposer order; public
cblock encodings are unchanged. A lane skipped inside a window stays skipped
until its next window, as Votor's parent rule requires. Other lanes continue,
and their accepted set may change between cblocks in the same window.

For lanes, each yardstick cblock additionally carries three `lanes` rows with
proposer, finalized/skipped/pending status, votes by kind and certificate kinds.
The existing aggregate votes and certificates describe execution finality.

| Scenario | Fault | Evidence | Lemma |
|---|---|---|---|
| `last-look` | proposer 3 commits 28 ms late | `late_block_dropped`, `skip_certificate` | Lemma 2.3 |
| `censorship` | proposers 1 and 2 leave a transaction out | `transaction_included` (carried by 3, finalized) | Lemma 2.2 |
| `crash` | proposer 3 sends nothing for the window | `two_proposers_continue` (P = {1,2}) | Theorem 2.1 |
| `equivocation` | proposer 3 signs two commitments for one slot | `slashing_proof` (both signatures), `skip_certificate` | Lemma 2.4 |
| `split-commitments` | proposer 3's commitment reaches only proposer 1 | `skip_certificate` | Lemma 2.2 |
| `private-front-run` | receiver 1 holds one fragment of a 2-of-3 transaction | `unreadable_fragment` (1 of 2), then the transaction executes | Section 3.2, Lemma 2.3 |

The table describes `today`. Under `agreement`, `split-commitments` instead
requires `window_continued`, naming the attack slot, `agreedSet` and
`finalized: true`. `last-look` keeps `late_block_dropped` and expects
`window_continued` in place of a skip. Crash, censorship, equivocation and
private-fragment evidence retain their meanings. Under `lanes`, split commitments
require `lane_skipped`, including the attacker's skip certificate and the other
finalized lanes. Last-look and equivocation retain their late-drop/slashing
evidence and use `lane_skipped` instead of a whole-cblock skip. Crash, censorship
and private-fragment evidence remain unchanged. Run the nine paired cases with
`node node/scripts/lab-agreement-check.mjs` from `backend/`; it prints each
run's yardstick and per-lane summary and removes its own temporary stores.

## CLI

| Command | |
|---|---|
| `acrux net up` / `acrux cluster up --hubs FRA,NYC,TYO` | a local Acrux net (`--genesis` names the genesis file) |
| `acrux keygen --out key.json` | a Solana keypair file; prints the address |
| `acrux send --keypair key.json --to <address> --lamports N [--n 3 --t 2]` | a plain or t-of-n transfer, followed to finality |
| `acrux node follow <rpc-url>` | follows a net from its latest finalized cblock: loads that cblock's state and checks its hash, then for every finalized slot checks each attestation's signature and Agreed(s), decrypts the agreed blocks with the released keys, checks them against the attested commitments, re-executes them on its own SVM and compares the state hash with the net's and with the finalization certificate |
| `acrux node propose --hub FRA --identity <keypair> [--net https://rpc.acrux.computer] [--genesis <file>]` | authenticates as an observer, receives its hub's existing signed protocol over public QUIC and re-executes finalized slots with matching state hashes; admission and block production follow in later steps |
| `acrux replay --rpc <solana-rpc> --slot <n>` or `--block <file>` | one Solana mainnet block (transaction versions legacy, 0 and 1) through Acrux's merge and Algorithm 4: the leader's order beside the merged order, each transaction's Acrux slot, track and start, how many Acrux slots the block takes, and the sandwiches in it (a trader's buy, a victim's buy of the same asset on the same pool, the trader's sale of the same amount at a profit), with what the trader kept and each sandwich repriced for Acrux: sent 2-of-3 the front leg has nothing to read, so the victim's output is recomputed with it taken out of the pool (a Pump.fun curve's virtual reserves from its trade events; a concentrated-liquidity pool's liquidity and price from its swap events, Meteora DAMM v2, Orca Whirlpool and Raydium CLMM; else the pool vaults on the constant-product rule where it reproduces every trade the block made there), and sent plain the trader's take less the cost of outbidding the victim's priority fee; `--json <file>` writes all of it as one record, `--save <file>` keeps the block to replay again |
| `acrux hub register <CODE> --latitude <deg> --longitude <deg>` | `register_hub` on the Hub Registry |
| `acrux hub join <CODE>`, `leave <CODE>`, `move <FROM> <TO>`, `release <CODE>` | the membership instructions and the permissionless release |
| `acrux schedule` | Algorithm 3 for hubs given as JSON on stdin (the API's schedule preview) |
| `acrux lab run` | one Fault Lab scenario |

Hub commands take `--identity key.json` (the validator identity that signs and pays), `--rpc` (an Acrux RPC such as https://rpc.acrux.computer for Acrux's registry, a Solana RPC for mainnet's; default the local Solana validator, http://127.0.0.1:4990) and `--program` (default `ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm`, the same on both chains).

## Observe a hub over QUIC

```text
acrux node propose --hub FRA --identity validator.json --genesis chain/genesis/acrux-net.json
```

`--identity` is a Solana keypair JSON file. `--net` defaults to
`https://rpc.acrux.computer`. The genesis, placement manifest and bridge rule
must match the net, just as for `node follow`. A new identity is an observer;
it sends only link frames until a finalized epoch snapshot admits it. Admitted
identities vote and produce their drawn blocks as described below.

The command bootstraps from the gateway's finalized `getExecutionSnapshot` and
`getHubPeers {hub,direct:true}`. The latter returns
`{hub,address,certificate,peers,relayPeers}`. The top-level address and base64 DER
certificate identify the hosted replay endpoint; `relayPeers` names the twelve
hosted relays. Each logical peer has an identity and ID, plus its own advertised
address and certificate once recorded on Acrux. Omitting `direct` keeps the legacy
response for genesis hubs; newly registered hubs are also found by `{hub}`. The observer
opens one authenticated connection, with the hub's logical links multiplexed on
it. The net challenges it with a fresh nonce; its identity signs a domain-bound
proof of the nonce, hub and public key. No observer consensus input is accepted.

For a hub with outside co-proposers, add `--peer-port <udp-port>` to open a
direct authenticated listener. `--peer-address <public-host:udp-port>` is required
when binding outside loopback; the same names are available as
`ACRUX_PEER_PORT` and `ACRUX_PEER_ADDRESS`. Outside-only hubs need these direct
links between their operators. Effective members automatically advertise the
address and pinned certificate in a signed `acrux-peer-v1:` Memo transaction on
Acrux. The identity pays the transaction fee and metadata-account rent from its
native wallet. The metadata is hashed state, and discovery follows the member's
effective hub; this requires no Hub Registry program upgrade.

After bootstrap, encrypted shreds, commitments, signed attestations, votes and
certificates arrive as the same `Envelope`/`Wire` messages used between validators.
For a genesis hub the observer receives its first hosted member's transcript;
an outside-only hub bootstraps from node zero's transcript. Both include the data
needed to re-execute every hub's finalized windows.
Mempool submissions are unnecessary: selected transactions and threshold
fragments already ride inside encrypted blocks. It checks the signed data,
reconstructs blocks, verifies agreement and finalization certificates, and uses
the follower's merge/execution path. It does not fetch follower RPC pages.

The prior in-process path shared its cblock preimage and local execution hashes.
The appended `Wire::Cblock` witness puts those values on the same signed wire.
Its preimage binds the accepted slots and end state to the certified cblock hash;
when bridging, that hash also includes the outbox root. No monotonic `Instant`
or local slot deadline crosses the wire. Existing message encodings, vote
signatures, state hashes and the RPC follower stay unchanged.

It prints the follower's `from cblock ... state ... matches` and
`slot ... re-executed ... state ... matches ... checked N` lines, plus each
logical co-proposer link and measured `RTT ... ms`. That RTT is the actual QUIC
round trip; `LocalTransport::link_delay` is not applied to external packets.

`acrux net up` listens on `ACRUX_PEER_PORT` / `--peer-port` (4940/udp by default),
using `ACRUX_BIND` (loopback by default). A public bind requires
`ACRUX_PEER_ADDRESS` / `--peer-address`, the public DNS name or IP with its UDP
port. Local cluster and Fault Lab runs do not open a public endpoint. Only this
single external port is published in compose; in-process validator ports remain
private. Host firewall instructions are in
[deploy/host/README.md](../deploy/host/README.md).

There are at most 32 authenticated external identities, one connection per
identity. TLS and the challenge must finish within five seconds. Authentication
and ping frames stay within 128 bytes, with at most ten pings per second. An
admitted identity can submit only a signed vote envelope, at most 2,048 bytes
and 32 votes per second; the boundary checks its identity, hub, epoch membership,
compact voter index, live slot range and both signatures. Other input is refused
and closes that exact connection, with the existing rate-limited warning.

Every link pulls the same history from its own cursor, woken by `published`.
History is bounded at 64 MiB and 262,144 frames. There is no per-peer packet queue.
Validator-set announcements and member votes use that same history path. A peer
that falls behind held history is closed. The replay process caps pending block
material at 64 MiB. A lost link or expired history ends the command with an error;
rerunning loads a fresh snapshot. `propose-observer-check.mjs --far` retains the
operator's 145 ms each-way RPC and QUIC proxy check.

After a release build and the `propose-peer-probe` example build, run
`node node/scripts/propose-observer-check.mjs` from `backend/`. It owns only
loopback ports 5041?5043, validator QUIC 5062?5073 and external QUIC 5074. It
checks at least 1,000 matching finalized slots, actual link RTT, rejection of a
bad challenge signature and disconnection of a non-reading peer while finality
keeps advancing. Its child processes and unique temporary store are cleaned.

## Admission at the bridge epoch boundary

`admissionFromSlot` is optional in the bridge rule. Without it, the 0.4.23
validator set, certificate weights, execution hashes and two-epoch stake draw
remain unchanged. Configure it as a bridge-epoch boundary at or after
`stakeFromSlot`; `epochSlots` must be divisible by 32. There is no default or
production activation slot in this release. When adding the rule to a running
chain, choose a future boundary; startup refuses a past activation missing its
committed marker. Distribute the same rule to every
validator and RPC/QUIC follower before activation.

Let A be its epoch. A keeps the legacy twelve-voter weights of 100,000,000 each
and the already frozen proposer draw, since Solana can already hold that set.
The first unified set is A+1. From A onward the cut-off is the start of the
middle proposer window, `e * epochSlots + epochSlots / 2`. Registry membership
(projected to e+1), delegated stake and the configured genesis weight are frozen
once from the state before that slot's transactions, for epoch e+1. If the slot
was skipped, the first later execution captures its inherited state before its
new transactions. A transaction at or after the cut-off waits another epoch.
The containing cblock must finalize before the snapshot is learned. Rollback
restores the hashed marker and derives the identical snapshot again.

That one snapshot supplies the epoch's `cap_stakes` / `build_schedule` draw,
certificate stake, compact vote indices and next-set bridge leaf. It replaces
the previous e+2 draw lead from A+1; no second stake snapshot is taken. Registry
joins, moves and leaves are interpreted at their effective epoch. A delegated
stake counts when `activation_epoch < e` and `deactivation_epoch >= e` for target
e. Outsiders need positive delegated stake; each genesis identity also keeps
`genesisStakeLamports`. A new hub needs at least four eligible members and capped
proposer stake of at least 1% of the snapshot's total stake. These conditions are
evaluated at the same mid-epoch cut-off, so an outside-only hub receives windows
from the next boundary after it qualifies. A late registration, join or delegation
waits for the following cut-off. Registry ECEF coordinates are frozen with that
epoch's set and supply the hub's regional placement. Genesis identities retain
their bootstrap protection; outside identities in an ineligible hub stay waiting.

The live bridge uses u32 vote bitmaps, so each snapshot admits at most 32 voters.
Eligible identities are selected by descending stake, then public-key bytes for
ties; excess identities remain waiting, with no fabricated stake or chain halt.
The final compact list follows the stable validator catalog. Catalog IDs never
change or get reused when an identity leaves; these wire IDs are distinct from
an epoch's compact voter indices.

An effective registry membership installs a rent-exempt Vote account even with
zero delegated stake. `validator_vote_address(identity)` keeps the existing
`["acrux_genesis_vote", identity]` seed and address; its neutral name works for
both genesis and outside identities. New identity/catalog/Vote-account rent
joins the native reserve. Vote roots and last-vote fields remain current as the
genesis refresh does, so stock `solana delegate-stake` accepts them. An outside
identity's first-time order is therefore: join, wait for effective membership
and its Vote account, delegate with stock tools, then wait for the admission
boundary after an eligible cut-off. Existing Vote PDAs and Stake accounts need
no migration.

```text
acrux hub join FRA --rpc https://rpc.acrux.computer --identity validator.json
solana --url https://rpc.acrux.computer --keypair wallet.json create-stake-account stake.json 2
solana --url https://rpc.acrux.computer --keypair wallet.json delegate-stake stake.json VOTE_ACCOUNT
acrux node propose --hub FRA --identity validator.json --genesis chain/genesis/acrux-net.json
```

Read the Vote address from `getVoteAccounts` or derive the PDA above.
`getVoteAccounts.current` includes installed Vote accounts and live delegation
weights, including those waiting for admission. `getValidatorSet {epoch?}`
returns the frozen set: `{epoch,cutoffSlot,members,catalog,genesisIdentities,hubs}`;
each member has identity, hub, decimal-string stake, `validatorKind` and
`admittedAtEpoch`. Omit epoch for the current set. `/hubs` uses this frozen set
for actual proposer weights and admission metadata. Each hub reports `hubCode`,
`active`, `formedOutside` and `activatedAtEpoch`. `formedOutside` means that its
current frozen members include no genesis identity. `activatedAtEpoch` records
the first active epoch for that registered hub, or null while waiting. The API
adds these fields to Acrux hubs and excludes inactive hubs from schedule previews.

Before its epoch starts the snapshot is announced as `Wire::ValidatorEpoch`
through the shared history; replay verifies it against finalized hashed state.
The voter installs the next set when replay finalizes its mid-epoch cut-off.
Hosted actors install that same finalized set at the cut-off and restore known
sets before resuming consensus. Early votes and certificates wait within the
existing 100,000-envelope / 64 MiB bounds. Neither a local block nor a timeout
can produce a vote for an epoch whose set is still unknown.
Wire votes, certificates and block material for an unknown set wait within the
observer's existing memory bounds, then undergo signature checks and counting
after installation. The bootstrap epoch keeps its identical inherited set.
`node propose` prints `epoch E observer ...` or `epoch E member ... voting index
N`, and each signed Notar/Finalize vote. It speculatively re-executes a witness
before voting, checks its state/outbox and safe parent, then signs with its own
identity. Finalized slots still print the same matching lines as `node follow`.
It writes exact signed vote envelopes durably before sending, in
`<identity-file>.acrux-votes.redb`. Keep that file with the identity across
restarts; its chain-scoped finalized anchor cannot regress, old finalized votes
are pruned, and conflicting initial/final votes cannot be signed on restart.
After a move, stop the old command and rerun it with the new `--hub`; the old
hub connection becomes an observer at the boundary. A drawn, admitted member
builds and signs its own blocks over that same link; admission alone does not
let it propose in another identity's role.

A missing outside voter contributes no stake. As outside stake approaches 20%,
80% fast finalization increasingly needs its participation; slow/fallback paths
can take longer or miss window deadlines. Without 60% participating stake,
certificates cannot form. This release does not hide offline stake or cap it to
keep genesis votes sufficient.

For a Solana registry identity, carry its effective membership with the same key:

```text
acrux hub carry FRA --rpc https://rpc.acrux.computer --solana-rpc SOLANA_RPC_URL --identity validator.json
```

The helper reads canonical finalized membership, Clock and hub accounts from
one Solana bank, checks owner, discriminator, identity, PDA/bump, hub id and the
requested code, then submits the existing Acrux `join_hub` signed by that same
identity. The corresponding hub must already exist on Acrux. This grants no
privileged membership: Acrux already permits any identity to sign its own join.
It cannot forge another identity's signature or silently carry a different hub.
No `hub_registry` upgrade or new placement is needed.

The two Rust admission checks cover the cut-off (including later joins, stake
changes, moves/leaves) and identical rollback/reload hashes. The stand check is
`chain/scripts/native-admission-check.mjs`; it requires the operator's fresh
local validator and existing native bridge bytecode at 4990. Its header lists
all explicit local rule values. It starts only loopback net/observer processes,
then proves a pre-boundary Solana hand-off, an outside Finalize signature counted
on Solana, and removal after leave. No validator is started or stopped by it.

## Proposing from an outside machine

Node 0.4.26 runs the same `proposer` packer, slice encryption/sealing, Rotor draw
and `ExchangeEngine` on the member's machine. The gateway routes submissions to
every selected role, including a direct identity-targeted record in the existing
64 MiB pull history. Those records enter only that member's own mempool. The
first twelve validators remain the Rotor relay pool in this step; they forward
outside shreds through the ordinary validator path. Fees and merge rules are
unchanged. Slot windows, blocks and transaction receivers explicitly report
`validatorKind: "genesis" | "external"`.

The hub's fixed genesis representative sends a signed `WindowStart` with the
window, certified parent and its Unix-microsecond start time. A member maps it
once to its local monotonic clock. Synchronize the member's system clock with
NTP: clock error and real link delay both spend its timing budget. The notice's
arrival never moves the start or extends a deadline. Blocks already past their
commit point are skipped. Finalized replay still verifies every state hash;
a separate authenticated commit lane keeps replay work off the hash exchange.

For FRA, delta-local is **10 ms**, so **2 delta-local is 20 ms**, inclusive from
the first authenticated commitment arrival or the proposer's own actual send.
Commit is scheduled at slot start + 50 ms. An arrival at +20.000 ms is on time;
+20.001 ms is late. A far operator does not receive a larger budget, and a late
block follows the same exclusion/skip rules as a late genesis block. Commitment receipt round trips are exported separately from this one-way
arrival deadline. Outside send offsets in the explorer's
slice and commitment records are zero (unknown); their host arrival offsets and
on-time decisions use actual socket receipt times.

The same identity journal also durably reserves each authored slot before its
commitment or shreds are sent. Restarting cannot seal a second block for that
slot. Keep the journal with the identity.

Clients normally use the public gateway. To accept direct submissions as well:

```text
acrux node propose --hub FRA --identity validator.json --genesis chain/genesis/acrux-net.json --rpc-port 5044
```

The optional listener uses `ACRUX_BIND` (loopback by default), bounded requests
and the same transaction validation. It accepts Solana `sendTransaction` and
Acrux plain/fragment submissions into this member's own mempool. Read executed
transactions and confirmations from the net's gateway. The public peer port is
still UDP 4940; no new host port is needed unless the operator exposes its own
submission listener.

`getExecutionAgreement {slot}` returns null until all twelve hosted validators
have executed that slot with one state hash, then
`{slot,stateHash,validators:12,matching:true,mismatches}`. It retains the last
2,400 completed slots and is an observation, outside consensus state.

The operator-run check is `chain/scripts/native-external-block-check.mjs`, next
to the admission check. Its header gives explicit local rule values. It admits a
member, waits up to 120 seconds for a drawn live window, submits a transfer via
the normal Solana surface, opens the accepted external block, checks both
co-proposer commitment arrivals, all twelve validator hashes and an independent
RPC follower. It uses only its own loopback processes and leaves the operator's
local Solana validator alone. No program rebuild or new placement is needed for this
step.

## Measured hash exchange

Node 0.4.28 exports actual Algorithm 1 commitment exchanges. A drawn co-proposer
returns a signed receipt for the original commitment's slot, proposer and hash.
The sender measures its round trip with its monotonic clock, matching the first
receipt once. Retransmissions do not add samples. These are round-trip
microseconds, not one-way delay estimates or the CLI's independent ping RTT.
No wall-clock subtraction, division by two or configured transport delay enters
the measurement. Proposing still requires the clock synchronization above.

An unordered pair is labelled `applied` when both endpoints are hosted genesis
validators using `LocalTransport::link_delay`, and `real` when an endpoint is an
outside member. Its two public addresses are sorted lexicographically. The
shared hosted engine pools its raw observations for each pair, so both hosted
directions count when available. Outside members also send identity-signed raw
observations over their authenticated uplink. The host validates their hub and
epoch membership and aggregates them for the same private API source, including
outside-only pairs. Direct co-proposer links pull bounded history without
simulated hub delay. The API does not average endpoint percentiles or connect to
operators' private metrics listeners.

`commitSpreadUs` keeps its existing series. New applied/real subset spreads use
one observer per slot: the lowest hosted co-proposer ID when present, otherwise
the lowest drawn co-proposer ID in an outside-only hub. Each subset
contains its own actual send and all peer arrivals of that link kind. A mixed
window can contribute both kinds. The subset becomes a sample when complete;
late commitments retain their actual arrival time, and a missing peer creates
no zero-valued sample. Deadline decisions never wait for telemetry.

Pair and spread statistics retain at most ten minutes and 4,096 observations per
series, with nearest-rank p50/p99 and retained sample counts. Empty classes are
null; pairs without recent observations are absent. Percentiles are calculated
from raw samples on a dedicated metrics worker. Protocol hooks submit through a
bounded, nonblocking 4,096-event channel; overload drops telemetry instead of
delaying a validator. The worker expires samples and publishes a cached snapshot
once a second. Readers copy that snapshot without locking validator state.

The net's existing private metrics port (4989 by default) serves:

- `GET /metrics`: labelled `acrux_hash_exchange_rtt_microseconds`,
  `acrux_hash_exchange_samples`, and `acrux_commit_spread_by_link_*` series.
- `GET /internal/hash-exchange`: JSON with `measuredAt`,
  `measurement: "roundTrip"`, per-hub pairs and spread subsets, and the combined
  `commitSpreadByLinkUs` summary.

The gateway's `getFacts` includes the same additive `hashExchange` and
`commitSpreadByLinkUs` fields. The API reads the private endpoint through its
existing `GATEWAY_INTERNAL_URL`, caches a successful read for two seconds, and
joins pairs to effective Acrux registry members. `/v1/facts` and
`/v1/hubs/FRA?chain=acrux` expose them. An unavailable or invalid private read
returns the existing retryable `NET_UNAVAILABLE` error after cache expiry.

An outside member can expose its own private measurements explicitly:

```text
acrux node propose --hub FRA --identity validator.json --genesis chain/genesis/acrux-net.json --metrics-port 5077
```

This optional listener uses `ACRUX_BIND`, loopback by default, and serves both
metrics paths above. It reports its own receipt observations; the site receives
their signed uplink reports through the host. Receipt telemetry stays out of hub mirrors and is
sent to an outside connection only after it demonstrates receipt support. The
shared pull history remains bounded at 64 MiB, with no outgoing per-peer queue.

The checks are `api/scripts/hash-exchange-check.mjs` (an owned unbridged local net
and API with a temporary local database) and
`chain/scripts/native-exchange-check.mjs` (the outside-member bridge stand,
operator-run with a local Solana validator). The second reuses the external-block
setup, keeps its direct submission on 5076, and checks the member's metrics on
5077. Both verify the API and private exports; the first is genesis-only and the
second also compares real loopback pairs with applied FRA pairs. The first runs
with default traffic and checks continued finalization and responsive API reads
after the net's fifteen-second warm-up.

## Measured

`acrux node propose` from a desktop about 290 ms from the host (CLI 0.4.23, 2026-11-19 UTC):
links to the four FRA co-proposers at a 291 to 296 ms round trip, 3,172 finalized slots
re-executed in 180 s with every state hash matching, at the net's own pace.

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
