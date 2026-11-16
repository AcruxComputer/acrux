// Sends N 2-of-3 transfers one after another on a running net and follows each to its end.
// Usage: node test/burst-e2e.mjs <rpc-url> <private faucet url> [n]
import { Keypair, SystemProgram, Transaction } from '@solana/web3.js'
import { connect } from '../dist/index.js'
const [rpcUrl, faucetUrl, count = '20'] = process.argv.slice(2)
const acrux = connect(rpcUrl)
const user = Keypair.generate()
const grant = await (await fetch(faucetUrl, { method: 'POST', body: JSON.stringify({ address: user.publicKey.toBase58(), amountLamports: '2000000000' }) })).json()
for await (const u of acrux.tx.follow(grant.transactionId)) if (['finalized', 'failed', 'expired'].includes(u.state)) break
const ids = []
for (let i = 0; i < Number(count); i++) {
  const tx = new Transaction({ feePayer: user.publicKey, recentBlockhash: (await acrux.latestBlockhash()).blockhash })
    .add(SystemProgram.transfer({ fromPubkey: user.publicKey, toPubkey: Keypair.generate().publicKey, lamports: 1_000_000 + i }))
  tx.sign(user)
  ids.push((await acrux.send(tx, { n: 3, t: 2, signer: user })).id)
  await new Promise(r => setTimeout(r, 1500))
}
const rpc = async (method, params) => (await (await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json()).result
const ends = []
for (const id of ids) {
  for (let tries = 0; ; tries++) {
    const state = (await rpc('getTransaction', { id }))?.state ?? 'unknown'
    if (['finalized', 'failed', 'expired'].includes(state) || tries > 120) { ends.push(state); break }
    await new Promise(r => setTimeout(r, 250))
  }
}
const tally = ends.reduce((a, s) => ({ ...a, [s]: (a[s] ?? 0) + 1 }), {})
console.log('sent', ids.length, JSON.stringify(tally))
process.exit(0)
