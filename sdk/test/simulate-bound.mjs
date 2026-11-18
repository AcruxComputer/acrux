// Review backlog: simulateTransaction runs on validator 0's own executor. One client asks for 100
// simulations of a program that burns its whole 1.4M CU budget, as fast as the read limit lets it.
// On 0.4.0 every one runs, and validator 0's thread spends that time away from the chain; from
// 0.4.1 a client's simulations are charged in compute units and the rest are refused.
// Usage: node test/simulate-bound.mjs <burn program id> <funded keypair file> [rpcUrl]
import { readFileSync } from 'node:fs'
import { ComputeBudgetProgram, Keypair, PublicKey, Transaction, TransactionInstruction } from '@solana/web3.js'

const [program, keypair, rpcUrl = 'http://127.0.0.1:4930'] = process.argv.slice(2)
const call = async (method, params) => (await (await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json())

const payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(keypair, 'utf8'))))
const { result: { value: { blockhash } } } = await call('getLatestBlockhash', [])
const tx = new Transaction({ feePayer: payer.publicKey, recentBlockhash: blockhash })
  .add(ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }))
  .add(new TransactionInstruction({ programId: new PublicKey(program), keys: [], data: Buffer.alloc(0) }))
tx.sign(payer)
const encoded = tx.serialize().toString('base64')

let ran = 0, refused = 0, units = 0
const started = performance.now()
for (let i = 0; i < 100; i++) {
  const answer = await call('simulateTransaction', [encoded, { encoding: 'base64', sigVerify: false, replaceRecentBlockhash: true }])
  if (answer.result) { ran++; units += answer.result.value.unitsConsumed ?? 0 }
  else refused++
}
const seconds = (performance.now() - started) / 1000
console.log(`100 simulations asked in ${seconds.toFixed(1)} s: ${ran} ran (${(units / 1e6).toFixed(1)}M CU on validator 0), ${refused} refused`)
process.exit(units <= 20_000_000 ? 0 : 1)
