# @acrux/sdk

Send a transaction into a leaderless Acrux slot from TypeScript. Runs in the browser and on Node 22.

## Install

The SDK builds on `@acrux/contract`. From this repository:

```bash
(cd contract && npm install && npm run build)
(cd sdk && npm install && npm run build)
npm install ./contract ./sdk
```

## Connect

```ts
import { connect } from '@acrux/sdk'

const acrux = connect('http://127.0.0.1:4930')
```

`connect` takes the gateway's JSON-RPC URL. The event stream is read one port above it; pass `{ wsUrl }` to name another.

## Send

Sign a Solana transaction over the net's blockhash, then send it.

```ts
const { blockhash } = await acrux.latestBlockhash()
// build and sign `tx` with @solana/web3.js: a Transaction, a VersionedTransaction, or its bytes

const { id } = await acrux.send(tx)
```

`send` targets a slot four slots ahead of the latest started one and returns `{ id, targetSlot }`.

## Threshold sends

```ts
const { id } = await acrux.send(tx, { n: 3, t: 2, signer: payer })
```

`n` is how many of the target slot's three proposers receive the transaction, and `t` is how many of them must include it before anyone can read it.

| Call | What is sent |
|---|---|
| `send(tx)` | the signed transaction to one proposer |
| `send(tx, { n: 3, t: 1 })` | the signed transaction to all three proposers |
| `send(tx, { n: 3, t: 2, signer })` | three encrypted fragments, readable from any two |
| `send(tx, { n: 3, t: 3, signer })` | three encrypted fragments, readable only from all three |

With `t` of 2 or 3 the fee payer signs the transaction's public header, so `send` takes the fee payer as `signer`: a `Keypair`, or a function `(message: Uint8Array) => Uint8Array | Promise<Uint8Array>` such as a wallet's `signMessage`.

## Follow a transaction

```ts
for await (const update of acrux.tx.follow(id)) {
  console.log(update.state, `${update.included} of ${update.threshold}`)
}
```

States arrive in order: `sent`, `included` (once per fragment a proposer includes), `executed`, then `finalized`, `failed` or `expired`, after which the loop ends. `update.transaction` is the contract's full `Transaction` record.

## Subscribe to slots

```ts
const stop = acrux.slots.subscribe(slot => console.log(slot.slot, slot.status))
stop()
```

The callback receives each slot when it starts and again when it executes.

## Errors

Every failure is an `AcruxError`, the contract's `ApiClientError`, whether the gateway refused the call or the SDK refused the input.

```ts
import { AcruxError } from '@acrux/sdk'

try {
  await acrux.send(tx, { n: 3, t: 2, signer: payer })
} catch (error) {
  if (error instanceof AcruxError && error.code === 'TRANSACTION_EXPIRED') {
    // the target slot started before the gateway admitted the send: sign and send again
  }
}
```

`error.code` is one of the contract's `ErrorCode` values, `error.body.error` carries `message`, `retryable` and `stage`.

## Everything else the gateway serves

`acrux.gateway` is the contract's gateway client: `acrux.gateway.call('getBalance', { address })`, `getSlot`, `getTransaction`, `getSchedule` and the full event stream through `acrux.gateway.subscribe`.

## Fragment format

`seal(bytes, header, headerSignature, t, receivers)` returns the `FragmentSubmission` objects `send` submits, and `open(fragments)` rebuilds the transaction from any `t` of them. `deriveHeader(bytes, targetSlot)` and `headerBytes(header)` give the public header and the bytes the fee payer signs. The wire layout is the one [`acrux-execution/src/threshold.rs`](../node/crates/acrux-execution/src/threshold.rs) decodes.

## Develop

```bash
npm install
npm test          # seal and open round trips, vectors from the Rust encoder
npm run e2e       # a plain and a 2-of-3 transfer on a running Acrux Net
```
