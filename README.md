# Acrux

New Constellation on Alpenglow, running.

Acrux is the first implementation of New Constellation (Kniep and Wattenhofer, 2026): every 50 ms slot, three co-located proposers each stream an encrypted block, exchange signed commitments within 2δlocal, then release their keys in attestations. Validators agree on the set P, decrypt, merge by priority across four tracks, execute on the SVM and vote once per four-slot cblock. Site: [acrux.computer](https://acrux.computer).

| Folder | What it is |
|---|---|
| `node/` | `acrux-node` (Rust): Algorithms 1 to 4 on Alpenglow, Acrux Net, the Fault Lab runner and the `acrux` CLI. See [node/README.md](node/README.md). |
| `chain/` | Anchor workspace: `hub_registry` (for Solana) and `demo_pool` (for Acrux Net's genesis). See [chain/README.md](chain/README.md). |
| `api/` | The API: TypeScript, Hono, Prisma 7 on Postgres. Registry read model, schedule preview, faucet, transaction lookup, lab runs, facts. |
| `contract/` | `@acrux/contract`: zod schemas and types for the API, the gateway's JSON-RPC and every event, plus clients. |
| `sdk/` | `@acrux/sdk`: `connect`, `send(tx, { n, t })`, `tx.follow`, `slots.subscribe`. See [sdk/README.md](sdk/README.md). |
| `spec/map.json` | Every algorithm line and lemma of the paper, mapped to file, function and Fault Lab scenario. |

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

`?fixture=<name>` (or `X-Fixture`) on the API in `MODE=fixture`, and the exports of `@acrux/contract/fixtures`, give every screen state its fixture: registry, slot, transaction, send, lab and connectivity states.

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
