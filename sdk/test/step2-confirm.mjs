// Road build 1, step 2: @solana/web3.js against Acrux with only the URL changed. A fresh key gets
// test ACRUX from the faucet; sendAndConfirmTransaction confirms a transfer over the WebSocket
// (signatureSubscribe), onAccountChange sees the recipient funded, onSlotChange counts slots.
// Usage: node test/step2-confirm.mjs [rpcUrl] [faucetUrl]
import { Connection, Keypair, LAMPORTS_PER_SOL, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'

const [rpc = 'https://rpc.acrux.computer', faucet = 'https://api.acrux.computer/v1/faucet'] = process.argv.slice(2)
const connection = new Connection(rpc, 'confirmed')
const payer = Keypair.generate()
const to = Keypair.generate().publicKey
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))

const body = faucet.includes('internal') ? { address: payer.publicKey.toBase58(), amountLamports: '2000000000' } : { address: payer.publicKey.toBase58() }
await fetch(faucet, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })
while ((await connection.getBalance(payer.publicKey)) === 0) await sleep(300)
console.log(`funded ${payer.publicKey.toBase58()}: ${(await connection.getBalance(payer.publicKey)) / LAMPORTS_PER_SOL}`)

let slots = 0
const slotSub = connection.onSlotChange(() => { slots++ })
const seen = new Promise(resolve => {
  const sub = connection.onAccountChange(to, account => { connection.removeAccountChangeListener(sub); resolve(account.lamports) }, { commitment: 'confirmed' })
})

const tx = new Transaction().add(SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: to, lamports: 1_000_000 }))
const started = performance.now()
const signature = await sendAndConfirmTransaction(connection, tx, [payer], { commitment: 'confirmed' })
console.log(`sendAndConfirmTransaction: ${signature} confirmed in ${Math.round(performance.now() - started)} ms`)
console.log(`onAccountChange: ${to.toBase58()} now holds ${(await seen) / LAMPORTS_PER_SOL}`)
await sleep(1000)
await connection.removeSlotChangeListener(slotSub)
console.log(`onSlotChange: ${slots} slots in the last second`)
process.exit(0)
