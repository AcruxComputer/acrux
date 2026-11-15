// One plain transfer and one 2-of-3 transfer on a running Acrux Net, each followed to finalized.
// Usage: node test/e2e.mjs [rpcUrl] [apiUrl]
import { Keypair, SystemProgram, Transaction, ComputeBudgetProgram, PublicKey } from '@solana/web3.js'
import { createClient } from '@acrux/contract'
import { connect } from '../dist/index.js'

const acrux = connect(process.argv[2] ?? 'http://127.0.0.1:4930')
const api = createClient(process.argv[3] ?? 'http://127.0.0.1:4920')
const payer = Keypair.generate()
const address = payer.publicKey.toBase58()

// An API in node mode answers with the grant; one still in fixture mode forwards the grant to the
// node and then fails to read the node's reply, so the balance is what tells.
const grant = await api.faucet({ address }, { fixture: 'normal' }).catch(error => ({ refused: error.code }))
console.log(JSON.stringify({ step: 'faucet', address, grant: grant.transactionId ?? grant.id ?? grant.refused }))
const funded = performance.now()
for (let spendable = 0n; spendable === 0n;) {
  if (performance.now() - funded > 30_000) throw new Error('The faucet did not fund the key within 30 s.')
  await new Promise(resolve => setTimeout(resolve, 200))
  spendable = BigInt((await acrux.gateway.call('getBalance', { address })).spendableLamports)
}
console.log(JSON.stringify({ step: 'funded', ms: Math.round(performance.now() - funded) }))

async function transfer(options) {
  const { blockhash } = await acrux.latestBlockhash()
  const tx = new Transaction({ feePayer: payer.publicKey, recentBlockhash: blockhash }).add(
    ComputeBudgetProgram.setComputeUnitLimit({ units: 20_000 }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 100 }),
    SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: Keypair.generate().publicKey, lamports: 10_000 }),
  )
  tx.sign(payer)
  const started = performance.now()
  const { id, targetSlot } = await acrux.send(tx, options)
  const trail = [{ state: 'submitted', ms: Math.round(performance.now() - started) }]
  let last
  for await (const update of acrux.tx.follow(id)) {
    trail.push({ state: update.state, included: `${update.included} of ${update.threshold}`, ms: Math.round(performance.now() - started) })
    last = update
  }
  console.log(JSON.stringify({ step: options ? `${options.t} of ${options.n}` : 'plain', id, targetSlot, executedSlot: last.transaction.executedSlot, result: last.transaction.result, failure: last.transaction.failure, trail }))
  if (last.state !== 'finalized') process.exitCode = 1
}

await transfer()
await transfer({ n: 3, t: 2, signer: payer })

let slots = 0
const stop = acrux.slots.subscribe(() => { slots += 1 })
await new Promise(resolve => setTimeout(resolve, 1000))
stop()
console.log(JSON.stringify({ step: 'slots.subscribe', callbacksInOneSecond: slots }))
