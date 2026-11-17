# @acrux/sdk

Send a transaction into a leaderless Acrux slot from TypeScript. Runs in the browser and on Node 20 or later.

## Install

```bash
npm install https://acrux.computer/downloads/acrux-sdk-0.1.2.tgz
```

The SDK's contract, `@acrux/contract` 0.1.10, installs with it from https://acrux.computer/downloads/acrux-contract-0.1.10.tgz.

## Connect

```ts
import { connect } from '@acrux/sdk'

const acrux = connect('https://rpc.acrux.computer')
```

`connect` takes the gateway's JSON-RPC URL. The event stream is read from `events.<domain>` when the URL is `rpc.<domain>` (`wss://events.acrux.computer` for Acrux Net), and one port above it for a node of your own (`http://127.0.0.1:4930` reads `ws://127.0.0.1:4931`); pass `{ wsUrl }` to name another.

## Send

Sign a Solana transaction over the net's blockhash, then send it.

```ts
const { blockhash } = await acrux.latestBlockhash()
// build and sign `tx` with @solana/web3.js: a Transaction, a VersionedTransaction, or its bytes

const { id } = await acrux.send(tx)
```

`send` targets a slot four slots ahead of the latest started one and returns `{ id, targetSlot }`. It reads the net in one round trip before it submits. A send that reaches the gateway after its target slot has started, from far away or after a wallet's approval, is admitted for up to 1,024 slots more and goes to the proposers then due, in the same roles.

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

Each fragment is submitted on its own, and one the gateway refuses is submitted once more. `send` returns as soon as `t` fragments are admitted, which is what the transaction needs to execute; follow its `id` from there. It throws only when fewer than `t` were admitted, and then the transaction never executes.

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
    // the blockhash or the target slot is over a minute old: build the transaction over a new
    // blockhash, sign it and send it
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
