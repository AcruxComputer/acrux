// Road build 1, step 3: calls a program deployed to Acrux with `solana program deploy` and prints
// its logs. Usage: node test/step3-call.mjs <programId> <keypair.json> [rpcUrl]
import { readFileSync } from 'node:fs'
import { Connection, Keypair, PublicKey, Transaction, TransactionInstruction, sendAndConfirmTransaction } from '@solana/web3.js'

const [programId, keypair, rpc = 'https://rpc.acrux.computer'] = process.argv.slice(2)
const connection = new Connection(rpc, 'confirmed')
const payer = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(keypair, 'utf8'))))
const tx = new Transaction().add(new TransactionInstruction({ programId: new PublicKey(programId), keys: [], data: Buffer.from('gm, Acrux') }))
const signature = await sendAndConfirmTransaction(connection, tx, [payer], { commitment: 'confirmed' })
const { meta } = await connection.getTransaction(signature, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })
console.log(`called ${programId}: ${signature}`)
for (const line of meta.logMessages) console.log(`  ${line}`)
process.exit(0)
