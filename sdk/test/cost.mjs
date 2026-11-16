// Measures what a send costs on a running net: lamports charged beyond the transfer, and the time
// from submit to executed and finalized, for plain sends and 2-of-3 threshold sends.
// Usage: node test/cost.mjs <rpc-url> <private faucet url> [rounds]
import { Keypair, SystemProgram, Transaction } from '@solana/web3.js'
import { connect } from '../dist/index.js'
const [rpcUrl, faucetUrl, rounds = '10'] = process.argv.slice(2)
const rpc = async (method, params) => (await (await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json()).result
const acrux = connect(rpcUrl)
const user = Keypair.generate()
const balance = async () => BigInt((await rpc('getBalance', { address: user.publicKey.toBase58() })).lamports)
const settle = async id => { for (;;) { const t = await rpc('getTransaction', { id }); if (t && ['finalized', 'failed', 'expired'].includes(t.state)) return t; await new Promise(r => setTimeout(r, 20)) } }
const grant = await (await fetch(faucetUrl, { method: 'POST', body: JSON.stringify({ address: user.publicKey.toBase58(), amountLamports: '2000000000' }) })).json()
await settle(grant.transactionId)
const kinds = { 'plain, 1 proposer': {}, 'plain, 3 proposers': { n: 3, t: 1 }, '2-of-3 threshold': { n: 3, t: 2, signer: user } }
const results = Object.fromEntries(Object.keys(kinds).map(k => [k, { fee: [], executed: [], finalized: [] }]))
for (let r = 0; r < Number(rounds); r++) for (const [kind, options] of Object.entries(kinds)) {
  const amount = 1_000_000n
  const tx = new Transaction({ feePayer: user.publicKey, recentBlockhash: (await acrux.latestBlockhash()).blockhash })
    .add(SystemProgram.transfer({ fromPubkey: user.publicKey, toPubkey: Keypair.generate().publicKey, lamports: Number(amount) }))
  tx.sign(user)
  const before = await balance(), start = Date.now()
  const { id } = await acrux.send(tx, options)
  let executedAt = null
  for (;;) { const t = await rpc('getTransaction', { id }); if (t?.state === 'executed' && !executedAt) executedAt = Date.now(); if (t && ['finalized', 'failed', 'expired'].includes(t.state)) { if (t.state !== 'finalized') throw new Error(`${kind} ${t.state}`); break } await new Promise(r => setTimeout(r, 10)) }
  const finalizedAt = Date.now()
  await new Promise(r => setTimeout(r, 300))
  results[kind].fee.push(Number(before - (await balance()) - amount))
  results[kind].executed.push((executedAt ?? finalizedAt) - start)
  results[kind].finalized.push(finalizedAt - start)
}
const median = a => [...a].sort((x, y) => x - y)[Math.floor((a.length - 1) / 2)]
for (const [kind, r] of Object.entries(results)) console.log(JSON.stringify({ kind, feeLamports: [...new Set(r.fee)], executedMs: median(r.executed), finalizedMs: median(r.finalized), n: r.fee.length }))
process.exit(0)
