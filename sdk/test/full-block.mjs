// Review B3: one client aims more than a block can hold at one slot. 960 distinct transfers, each
// padded with a memo to Solana's 1,232-byte limit, from eight payers, all to the same target slot
// and its three proposers. On 0.4.0 the proposer blocks pass 1 MiB and the net process exits. From
// 0.4.1 the proposers pack what fits and carry the rest in the following slots: the net keeps
// running and every transfer executes.
// Usage: node test/full-block.mjs [rpcUrl] [private faucet url]   (run the net with ACRUX_SUBMIT_RATE=1000)
import { Keypair, PublicKey, SystemProgram, Transaction, TransactionInstruction } from '@solana/web3.js'
import { deriveHeader } from '../dist/index.js'

const [rpcUrl = 'http://127.0.0.1:4930', faucetUrl = 'http://127.0.0.1:4989/internal/faucet'] = process.argv.slice(2)
const MEMO = new PublicKey('MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr')
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const call = async (method, params) => {
  const body = await (await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json()
  if (body.error) throw new Error(`${method}: ${JSON.stringify(body.error)}`)
  return body.result
}

const payers = Array.from({ length: 8 }, () => Keypair.generate())
for (const payer of payers) {
  await fetch(faucetUrl, { method: 'POST', body: JSON.stringify({ address: payer.publicKey.toBase58(), amountLamports: '10000000000' }) })
}
for (const payer of payers) while ((await call('getBalance', [payer.publicKey.toBase58()])).value === 0) await sleep(300)
const to = Keypair.generate().publicKey
const skipsBefore = (await call('getCounters', {})).skips
const { value: { blockhash } } = await call('getLatestBlockhash', [])
const target = (await call('getSlot', [])) + 40

const padding = Buffer.alloc(960, 'acrux')
const submissions = []
payers.forEach((payer, p) => {
  for (let i = 0; i < 120; i++) {
    const tx = new Transaction({ feePayer: payer.publicKey, recentBlockhash: blockhash })
      .add(SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: to, lamports: 1_000_000 + p * 1000 + i }))
      .add(new TransactionInstruction({ programId: MEMO, keys: [], data: padding }))
    tx.sign(payer)
    const bytes = tx.serialize()
    submissions.push({ signature: tx.signatures[0].signature, params: { header: deriveHeader(bytes, target), transaction: bytes.toString('base64'), receivers: [1, 2, 3] } })
  }
})
console.log(`${submissions.length} transactions of ${submissions[0].params.transaction.length * 3 / 4} bytes, all to slot ${target}`)

let refused = 0
for (let i = 0; i < submissions.length; i += 60) {
  await Promise.all(submissions.slice(i, i + 60).map(s => call('submitTransaction', s.params).catch(() => refused++)))
}
console.log(`submitted, ${refused} refused at admission`)

// Every transfer must execute while the net keeps producing; two minutes at most.
const ids = submissions.map(s => s.params.header.id)
const done = new Set()
let slot = 0
for (const deadline = Date.now() + 120_000; Date.now() < deadline && done.size < ids.length; ) {
  await sleep(2000)
  slot = await call('getSlot', []).catch(() => null)
  if (slot === null) break
  for (const id of ids.filter(id => !done.has(id))) {
    const tx = await call('getTransaction', { id }).catch(() => null)
    if (tx && ['executed', 'finalized', 'failed'].includes(tx.state)) done.add(id)
  }
}
const alive = (await call('getSlot', []).catch(() => null)) !== null
const skips = alive ? (await call('getCounters', {})).skips - skipsBefore : null
console.log(`net ${alive ? `running at slot ${slot}` : 'stopped'} (target ${target}): ${done.size} of ${ids.length} executed, ${skips ?? '?'} skipped cblocks while it carried them`)
process.exit(alive && done.size === ids.length ? 0 : 1)
