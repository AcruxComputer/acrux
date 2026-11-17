// Sends from a client far from the gateway: every JSON-RPC call goes through a local proxy that
// holds it DELAY ms each way (145 ms each way is the 290 ms round trip measured to the public host).
// A plain transfer, a 2-of-3 transfer, and a 2-of-3 transfer whose signer takes 3 s like a wallet
// approval, each followed to finalized.
// Usage: node test/far-e2e.mjs [sdkModule] [rpcUrl] [wsUrl] [faucetUrl]
import http from 'node:http'
import { Keypair, SystemProgram, Transaction, ComputeBudgetProgram } from '@solana/web3.js'
import { ed25519 } from '@noble/curves/ed25519.js'

const [sdkModule = '../dist/index.js', rpcUrl = 'http://127.0.0.1:4930', wsUrl = 'ws://127.0.0.1:4931', faucetUrl = 'http://127.0.0.1:4989/internal/faucet'] = process.argv.slice(2)
const DELAY = Number(process.env.DELAY ?? 145)
const { connect } = await import(sdkModule)

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const proxy = http.createServer((req, res) => {
  let body = ''
  req.on('data', chunk => { body += chunk })
  req.on('end', async () => {
    await sleep(DELAY)
    const answer = await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body })
    const text = await answer.text()
    await sleep(DELAY)
    res.writeHead(answer.status, { 'content-type': 'application/json' }).end(text)
  })
})
await new Promise(resolve => proxy.listen(0, '127.0.0.1', resolve))
const acrux = connect(`http://127.0.0.1:${proxy.address().port}`, { wsUrl })

const payer = Keypair.generate()
const address = payer.publicKey.toBase58()
await fetch(faucetUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ address, amountLamports: '2000000000' }) })
for (const started = Date.now(); BigInt((await acrux.gateway.call('getBalance', { address })).spendableLamports) === 0n;) {
  if (Date.now() - started > 30_000) throw new Error('The faucet did not fund the key within 30 s.')
  await sleep(200)
}

async function transfer(label, options) {
  const { blockhash } = await acrux.latestBlockhash()
  const tx = new Transaction({ feePayer: payer.publicKey, recentBlockhash: blockhash }).add(
    ComputeBudgetProgram.setComputeUnitLimit({ units: 20_000 }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 100 }),
    SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: Keypair.generate().publicKey, lamports: 10_000 }),
  )
  tx.sign(payer)
  const started = Date.now()
  try {
    const { id } = await acrux.send(tx, options)
    let last
    for await (const update of acrux.tx.follow(id)) last = update
    return { label, id: id.slice(0, 12), state: last.state, result: last.transaction.result, ms: Date.now() - started }
  } catch (error) {
    return { label, refused: error.code ?? error.message, ms: Date.now() - started }
  }
}

const wallet = async message => { await sleep(3_000); return ed25519.sign(message, payer.secretKey.subarray(0, 32)) }
const results = [
  await transfer('plain'),
  await transfer('2 of 3', { n: 3, t: 2, signer: payer }),
  await transfer('2 of 3, signer takes 3 s', { n: 3, t: 2, signer: wallet }),
]
for (const r of results) console.log(JSON.stringify(r))
proxy.close()
process.exit(results.every(r => r.state === 'finalized' && r.result === 'success') ? 0 : 1)
