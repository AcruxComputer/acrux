// Road build 1, step 7: the SPL programs at their mainnet addresses on Acrux, each driven by its own
// SDK with only the RPC URL changed. @solana/spl-token makes an SPL Token mint, its associated
// token accounts, a mint and a transfer; then a Token-2022 mint with a transfer fee, a transfer
// that withholds the fee; then a memo to Memo v3 (MemoSq4…) and to Memo v1. Mainnet's newer memo
// program (Memo4c2p…, the one @solana/spl-memo 0.3 sends to) comes with the placements.
// Usage: node test/step7-spl.mjs [rpcUrl] [faucetUrl] [funded keypair file, used instead of the faucet]
import { readFileSync } from 'node:fs'
import { Connection, Keypair, PublicKey, SystemProgram, Transaction, TransactionInstruction, sendAndConfirmTransaction } from '@solana/web3.js'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID, ExtensionType, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
  createInitializeMintInstruction, createInitializeTransferFeeConfigInstruction, createMint, getAccount,
  getMintLen, getOrCreateAssociatedTokenAccount, getTransferFeeAmount, mintTo, transferChecked, transferCheckedWithFee,
} from '@solana/spl-token'

const [rpc = 'https://rpc.acrux.computer', faucet = 'https://api.acrux.computer/v1/faucet', funded] = process.argv.slice(2)
const connection = new Connection(rpc, 'confirmed')
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const payer = funded ? Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(funded, 'utf8')))) : Keypair.generate()
const recipient = Keypair.generate().publicKey
const MEMO_V1 = new PublicKey('Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo')
const MEMO_V3 = new PublicKey('MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr')
const show = (program, what, signature) => console.log(`${program.padEnd(26)} ${what.padEnd(44)} ${signature}`)

if (!funded) {
  const body = faucet.includes('internal') ? { address: payer.publicKey.toBase58(), amountLamports: '2000000000' } : { address: payer.publicKey.toBase58() }
  const grant = await fetch(faucet, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })
  if (!grant.ok) throw new Error(`faucet refused: ${await grant.text()}`)
}
while ((await connection.getBalance(payer.publicKey)) === 0) await sleep(300)
console.log(`payer ${payer.publicKey.toBase58()}, recipient ${recipient.toBase58()}\n`)

// SPL Token and the Associated Token Account program.
const mint = await createMint(connection, payer, payer.publicKey, null, 6, Keypair.generate(), { commitment: 'confirmed' }, TOKEN_PROGRAM_ID)
show('SPL Token', `createMint ${mint.toBase58()}`, '')
const from = await getOrCreateAssociatedTokenAccount(connection, payer, mint, payer.publicKey)
const to = await getOrCreateAssociatedTokenAccount(connection, payer, mint, recipient)
show('Associated Token Account', `two token accounts, ${to.address.toBase58().slice(0, 8)}… the recipient's`, '')
show('SPL Token', 'mintTo 1,000', await mintTo(connection, payer, mint, from.address, payer, 1_000_000_000))
show('SPL Token', 'transferChecked 250', await transferChecked(connection, payer, from.address, mint, to.address, payer, 250_000_000, 6))
const held = [(await getAccount(connection, from.address)).amount, (await getAccount(connection, to.address)).amount]
show('SPL Token', `balances ${Number(held[0]) / 1e6} and ${Number(held[1]) / 1e6}`, '')
if (held[0] !== 750_000_000n || held[1] !== 250_000_000n) throw new Error('SPL Token balances are not 750 and 250')

// Token-2022 with a transfer fee of 1% (at most 10 tokens).
const mint22 = Keypair.generate()
const space = getMintLen([ExtensionType.TransferFeeConfig])
const create22 = new Transaction().add(
  SystemProgram.createAccount({ fromPubkey: payer.publicKey, newAccountPubkey: mint22.publicKey, space, lamports: await connection.getMinimumBalanceForRentExemption(space), programId: TOKEN_2022_PROGRAM_ID }),
  createInitializeTransferFeeConfigInstruction(mint22.publicKey, payer.publicKey, payer.publicKey, 100, 10_000_000n, TOKEN_2022_PROGRAM_ID),
  createInitializeMintInstruction(mint22.publicKey, 6, payer.publicKey, null, TOKEN_2022_PROGRAM_ID),
)
show('Token-2022', `mint with a 1% transfer fee ${mint22.publicKey.toBase58()}`, await sendAndConfirmTransaction(connection, create22, [payer, mint22]))
const from22 = await getOrCreateAssociatedTokenAccount(connection, payer, mint22.publicKey, payer.publicKey, false, 'confirmed', undefined, TOKEN_2022_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID)
const to22 = await getOrCreateAssociatedTokenAccount(connection, payer, mint22.publicKey, recipient, false, 'confirmed', undefined, TOKEN_2022_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID)
show('Token-2022', 'mintTo 1,000', await mintTo(connection, payer, mint22.publicKey, from22.address, payer, 1_000_000_000, [], undefined, TOKEN_2022_PROGRAM_ID))
show('Token-2022', 'transferCheckedWithFee 100, fee 1', await transferCheckedWithFee(connection, payer, from22.address, mint22.publicKey, to22.address, payer, 100_000_000n, 6, 1_000_000n, [], undefined, TOKEN_2022_PROGRAM_ID))
const received = await getAccount(connection, to22.address, 'confirmed', TOKEN_2022_PROGRAM_ID)
show('Token-2022', `recipient holds ${Number(received.amount) / 1e6}, withheld ${Number(getTransferFeeAmount(received)?.withheldAmount ?? 0n) / 1e6}`, '')
if (received.amount !== 99_000_000n || getTransferFeeAmount(received)?.withheldAmount !== 1_000_000n) throw new Error('Token-2022 did not withhold the fee')

// Memo v3, signed by the payer, and v1 beside it.
const memo = await sendAndConfirmTransaction(connection, new Transaction().add(new TransactionInstruction({ programId: MEMO_V3, keys: [{ pubkey: payer.publicKey, isSigner: true, isWritable: false }], data: Buffer.from('gm from Acrux') })), [payer])
const memoV1 = await sendAndConfirmTransaction(connection, new Transaction().add(new TransactionInstruction({ programId: MEMO_V1, keys: [], data: Buffer.from('gm from Acrux, v1') })), [payer])
for (const [signature, version] of [[memo, 'v3 MemoSq4g…'], [memoV1, 'v1 Memo1Uhk…']]) {
  const tx = await connection.getTransaction(signature, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })
  const line = tx?.meta?.logMessages?.find(l => l.includes('Memo (len'))
  if (!tx || tx.meta?.err) throw new Error(`memo ${version} did not execute`)
  show(`Memo ${version}`, line ? line.replace('Program log: ', '') : 'executed', signature)
}
console.log('\nSPL Token, Token-2022, Associated Token Account and Memo v1 and v3 ran at their mainnet addresses')
process.exit(0)
