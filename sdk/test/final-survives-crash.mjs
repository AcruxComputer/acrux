// Review B3: a transfer reported finalized must survive a crash. Run against a net started with
// ACRUX_STATE_WRITE_DELAY_MS (a slow disk): `send` funds a key, sends a transfer with stock web3.js
// at `finalized` and prints its signature and recipient the moment it is reported final; the
// caller kills the net at once and starts it again on the same store; `check` then reads the
// transfer back. On 0.4.0 the restart resumes from a state written before the transfer and it is
// gone; from 0.4.1 a cblock is reported final only once its state is on disk.
// Usage: node test/final-survives-crash.mjs send [rpcUrl] [private faucet url]
//        node test/final-survives-crash.mjs check <signature> <recipient> [rpcUrl]
import { Connection, Keypair, LAMPORTS_PER_SOL, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'

const [mode, ...rest] = process.argv.slice(2)
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))

if (mode === 'send') {
  const [rpcUrl = 'http://127.0.0.1:4930', faucetUrl = 'http://127.0.0.1:4989/internal/faucet'] = rest
  const connection = new Connection(rpcUrl, 'finalized')
  const payer = Keypair.generate()
  await fetch(faucetUrl, { method: 'POST', body: JSON.stringify({ address: payer.publicKey.toBase58(), amountLamports: String(LAMPORTS_PER_SOL) }) })
  while ((await connection.getBalance(payer.publicKey, 'finalized')) === 0) await sleep(300)
  // The grant itself is on disk well before the transfer goes.
  await sleep(15_000)
  const to = Keypair.generate().publicKey
  const tx = new Transaction().add(SystemProgram.transfer({ fromPubkey: payer.publicKey, toPubkey: to, lamports: 5_000_000 }))
  const started = performance.now()
  const signature = await sendAndConfirmTransaction(connection, tx, [payer], { commitment: 'finalized' })
  console.log(`${signature} ${to.toBase58()} ${Math.round(performance.now() - started)}`)
  process.exit(0)
}

const [signature, recipient, rpcUrl = 'http://127.0.0.1:4930'] = rest
const connection = new Connection(rpcUrl, 'finalized')
const balance = await connection.getBalance(new (await import('@solana/web3.js')).PublicKey(recipient), 'finalized')
const { value: [status] } = await connection.getSignatureStatuses([signature], { searchTransactionHistory: true })
const kept = balance === 5_000_000
console.log(`after the restart: recipient holds ${balance} lamports, status ${status?.confirmationStatus ?? 'unknown'}: ${kept ? 'the transfer survived' : 'the transfer is gone'}`)
process.exit(kept ? 0 : 1)
