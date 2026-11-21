# Acrux

Solana at 50 ms.

Acrux is a Solana fork with New Constellation consensus (Kniep and Wattenhofer, 2026): Solana's transactions and programs on the SVM, ordered by concurrent proposers. Every 50 ms slot, three co-located proposers each stream an encrypted block, exchange signed commitments within 2δlocal, then release their keys in attestations. Validators agree on the set P, decrypt, merge by priority across four tracks, execute and vote once per four-slot cblock. ACRUX is the chain's native token ([node/README.md](node/README.md#acrux)). Site: [acrux.computer](https://acrux.computer).

| Folder | What it is |
|---|---|
| `node/` | `acrux-node` (Rust): Algorithms 1 to 4 on Alpenglow, Acrux Net, the Fault Lab runner and the `acrux` CLI. See [node/README.md](node/README.md). |
| `chain/` | Anchor workspace: `hub_registry` (on Solana and in Acrux's genesis) and `demo_pool` (in Acrux's genesis). See [chain/README.md](chain/README.md). |
| `api/` | The API: TypeScript, Hono, Prisma 7 on Postgres. Registry read model, schedule preview, transaction lookup, the bridge and burn read models, lab runs, facts; its keeper role runs the bridge relayer and the ACRUX buyback. |
| `contract/` | `@acrux/contract`: zod schemas and types for the API, the gateway's JSON-RPC and every event, plus clients. |
| `sdk/` | `@acrux/sdk`: `connect`, `send(tx, { n, t })`, `tx.follow`, `slots.subscribe`. See [sdk/README.md](sdk/README.md). |
| `spec/map.json` | Every algorithm line and lemma of the paper, mapped to file, function and Fault Lab scenario. |
| `deploy/` | The node and API images and the single-host stack. See [deploy/README.md](deploy/README.md). |

## Run locally

| Service | Port | Start |
|---|---|---|
| Postgres | 127.0.0.1:4921 | `docker run -d --name acrux-postgres -p 127.0.0.1:4921:5432 -e POSTGRES_USER=acrux -e POSTGRES_PASSWORD=acrux -e POSTGRES_DB=acrux postgres:17-alpine` |
| Local Solana validator, Hub Registry | 4990 RPC, 4991 WebSocket, 4992 faucet, 4993 gossip | `bash chain/scripts/validator.sh chain/.ledger 75 &` then `node chain/scripts/init-localnet.mjs` (after the chain build in [chain/README.md](chain/README.md)) |
| Acrux Net | 4930 JSON-RPC, 4931 events, 4989 metrics (private), 4950-4961 QUIC | `cargo build --release -p acrux-node`, then `./target/release/acrux net up` from `node/` |
| API | 4920 | `cd api && npm install && npm run build && npm start` with `MODE=node` (`.env.example` lists every name; build `contract/` first) |
| Fault Lab sandboxes | 5016-5027 | started by the API, one run at a time |

A start of the net continues its chain from the last finalized cblock (`--fresh` begins a new one). `npm start` in `api/` applies migrations, then serves.

## Consume the contract

`npm run build` in `contract/` compiles it and writes `openapi.json`. The API and the SDK depend on it as `file:../contract`.

```ts
import { createClient, createGatewayClient } from '@acrux/contract'

const api = createClient('http://127.0.0.1:4920')
const net = createGatewayClient({ rpcUrl: 'http://127.0.0.1:4930', wsUrl: 'ws://127.0.0.1:4931' })
const hubs = await api.hubs()
const facts = await net.call('getFacts', {})
const stream = net.subscribe({ kind: 'all' }, event => { /* switch on event.type */ })
```

Methods with no parameters take `{}`. Every response and event parses under the contract's schemas, and failures arrive as `ApiClientError` with the error envelope in `body.error`. The API also serves `/openapi.json`, which carries the gateway's JSON-RPC and event stream as `x-acrux-json-rpc` and `x-acrux-event-stream`.

Response and event objects strip unknown fields, so clients can read additive
server changes with an older contract. Request bodies, query parameters, stream
commands and JSON-RPC requests remain strict, including nested submission headers.
Run `node scripts/response-compatibility-check.mjs` from `contract/` after its build
to check the response fixtures and strict request boundaries.

`?fixture=<name>` (or `X-Fixture`) on the API in `MODE=fixture`, and the exports of `@acrux/contract/fixtures`, give every screen state its fixture: registry, slot, transaction, send, lab and connectivity states.

## Burn read model

`GET /v1/burn?limit=24&cursor=<epoch>` publishes the Token page's ACRUX supply and
burn history. `createClient(...).burn()` parses its additive `Burn` contract.
Epochs run newest first from the native cut-over; `epochs.page.nextCursor` is an
exclusive epoch number, not an offset. Before the cut-over, the list is empty.

- `mintSupply` is Solana's finalized `getTokenSupply` amount, decimals and actual
  `sampledAt`. A single in-flight request and a 60-second cache share the API's
  existing paced Solana RPC budget.
- `totals.baseFeeBurnBaseUnits` counts the relayer journal's finalized Solana
  burns; `totals.buybackBurnBaseUnits` is the creator-fee burn total from
  `/v1/buyback`. Both are decimal integer strings in the mint's base units.
- Each epoch gives its Acrux slot interval, observed UTC boundaries, raw burned
  base fees in native lamports, the certified `FeeBurn` amount in token base
  units and its sub-unit remainder. `solanaBurn.state` is `not_yet_due`, `pending`
  or `burned`; its signature comes from the relayer's signed transaction journal.
- `buybackBurnBaseUnits` sums confirmed buyback `burn` rows with `createdAt` in
  the epoch's half-open UTC interval. A running epoch has no end time or end
  supply sample. Unknown historical intervals have a null buyback sum.
- `mintSupplyAtEnd` is the finalized supply first observed by this API after the
  epoch closes, with its actual observation time. It is saved once in Postgres
  and stays unchanged as live supply moves. A late first read is an observation
  made then, not a reconstructed historical supply quote.

The gateway's additive `getFeeBurns { cursor?, limit? }` reads finalized
accounting observations committed beside its hashed execution checkpoint in
`state.redb`. It does not change consensus or follower re-execution. The prior
node retained only the current fee counter and last closure, not full raw sums
or UTC boundaries: unavailable historical fields are `null`, never fabricated
zeroes. Complete observations are retained from this version onward; a closed
archived proof can also recover its certified rounded amount.

Use the existing `ACRUX_MINT`, `LAUNCH_WALLET`, `BRIDGE_PROGRAM_ID`,
`GATEWAY_RPC_URL`, `SOLANA_RPC_URL` and `DATABASE_URL`; no new environment setting
is needed. Apply the `burn_supply_samples` migration before starting the API
(`npm.cmd run db:migrate` in `api/`). An unset mint gives null supply and an empty
list. The contract's `burnNoEpochs`, `burnPending`, `burnBurned` and `burnRunning`
exports, also selected with `fixture=burn-no-epochs`, `burn-pending`,
`burn-burned` and `burn-running`, cover the Token page's states.

## Event catalogue

Every event is `{ version: 1, sequence, streamId, type, slot, anchor, atUs, data, node? }`. `sequence` rises within a stream; a new `streamId` invalidates a cursor. `node` is the validator identity that observed the event. `slot` is null only for control and window messages. Each slot has one UTC `wallTime`. Amounts and stake are decimal strings, `priorityFeePerCu` is micro-lamports per CU, hub ECEF is integer metres in WGS84. Proposer and track indices start at 1, slice and fragment indices and merged positions at 0. A null entry of `pi` is the paper's bottom symbol, `agreedSet` is P.

| Event type | Data |
|---|---|
| `stream.snapshot` | The complete current view, for a new connection |
| `stream.resync` | Reason and a replacement snapshot |
| `stream.heartbeat` | Latest slot and `live` / `stalled` |
| `stream.error` | The API error envelope |
| `window.started` | Hub, slot range, consensus slots, the three proposers |
| `slot.started` | Pending slot, wall time and window |
| `block.slice` | Proposer, slice index, bytes, 64 shreds of which 32 rebuild it |
| `commitment.sent` | Signed commitment, block hash and key hash |
| `commitment.arrived` | Sender, receiver, receiver's `start[s]`, on time against 2δlocal |
| `attestation.sent` | Signed vector π and the released key |
| `attestation.arrived` | Arrival of an attestation at a validator |
| `slot.agreed` | The agreed set P |
| `slot.skipped` | Reason, vectors and the skipped range |
| `block.decrypted` | Proposer and transaction ids |
| `slot.merged` | The merged order with source proposers and fragments |
| `slot.scheduled` | Algorithm 4's four-track placements and gaps |
| `slot.executed` | The executed slot and its state hash |
| `cblock.assembled` | Four xblocks, parent and state commitment |
| `cblock.vote` | Kind, voter, stake share, cblock hash |
| `cblock.certificate` | Notarization, notar-fallback, fast or slow finalization, or skip |
| `cblock.finalized` | The finalized cblock |
| `transaction.updated` | The transaction's trail: sent, included, executed, finalized, failed, expired |
| `net.counters` | Slots, finalized cblocks, skips, average commit spread |
| `lab.updated` | A lab run's status and outcome |
| `lab.evidence` | One piece of a scenario's evidence |

`anchor` names what `atUs` counts from: slot start, the window's ParentReady, the cblock's first slot, or the stream. Commitment `relativeToStartUs` counts from the receiver's own `start[s]`, the earlier of its own commitment and the first one received.

WebSocket commands: `{ version: 1, type: 'subscribe', filter: { kind: 'all' } }`, `{ kind: 'slots', from, to }` or `{ kind: 'transaction', id }`, with optional `afterSequence` and `streamId` for retained history; `{ version: 1, type: 'unsubscribe' }` stops delivery. The gateway keeps the last 24,000 events, `getRecentSlots` returns the last 64 slots whole, and `getSlot` reads older slots from the store.

A lab run's `/v1/lab/runs/:id/events` returns `{ run, events }` in the same schema; `/stream` sends them as SSE (event name the type, id the sequence) and resumes from `Last-Event-ID`.
