// confidential_deposit on the local validator: a deposit through the adapter lands in the bridge
// with the adapter PDA as depositor, never the user's wallet. Not part of the default run; run it with
//   node tests/run.mjs tests/confidential_deposit.test.mjs
// after the steps in docs/confidential-deposit-e2e.md. Written without a validator to run it on:
// treat the first run as the first real test of this file.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { before, test } from 'node:test'
import anchor from '@coral-xyz/anchor'
import { TOKEN_PROGRAM_ID, createMint, getAccount, getOrCreateAssociatedTokenAccount, mintTo } from '@solana/spl-token'
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { chainDir, devKeypair } from '../scripts/dev-keys.mjs'
import { RPC_URL } from '../scripts/registry.mjs'
import { EPOCH_SLOTS } from '../scripts/bridge-relay.mjs'

const load = (name) => JSON.parse(readFileSync(join(chainDir, `target/idl/${name}.json`), 'utf8'))
const connection = new Connection(RPC_URL, 'confirmed')
const authority = devKeypair('local-authority')
const user = Keypair.generate()
const provider = new anchor.AnchorProvider(connection, new anchor.Wallet(user), { commitment: 'confirmed' })
const bridgeProgram = new anchor.Program(load('acrux_bridge'), provider)
const adapterProgram = new anchor.Program(load('confidential_deposit'), provider)
const pdaOf = (program, ...seeds) => PublicKey.findProgramAddressSync(seeds, program.programId)[0]
const bridge = pdaOf(bridgeProgram, Buffer.from('bridge'))
const adapter = pdaOf(adapterProgram, Buffer.from('adapter'))
const escrowOf = (mint) => pdaOf(adapterProgram, Buffer.from('escrow'), mint.toBuffer())
const vaultOf = (mint) => pdaOf(bridgeProgram, Buffer.from('vault'), mint.toBuffer())
const recordOf = (n) => pdaOf(bridgeProgram, Buffer.from('deposit'), new anchor.BN(n).toArrayLike(Buffer, 'le', 8))
const eventAuthority = pdaOf(bridgeProgram, Buffer.from('__event_authority'))
let mint, source

const viaAdapter = (amount, recipient, { mintKey = mint, from = source, sequence }) =>
  adapterProgram.methods.deposit(new anchor.BN(amount.toString()), recipient).accounts({
    user: user.publicKey, adapter, mint: mintKey, source: from, escrow: escrowOf(mintKey),
    bridge, vault: vaultOf(mintKey), record: recordOf(sequence),
    bridgeEventAuthority: eventAuthority, bridgeProgram: bridgeProgram.programId,
    tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }).rpc()

before(async () => {
  await sendAndConfirmTransaction(connection, new Transaction().add(
    SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: user.publicKey, lamports: 10 * LAMPORTS_PER_SOL }),
  ), [authority])
  mint = await createMint(connection, user, user.publicKey, null, 6)
  source = (await getOrCreateAssociatedTokenAccount(connection, user, mint, user.publicKey)).address
  await mintTo(connection, user, mint, source, user, 10_000_000_000n)
  await bridgeProgram.methods.initialize(new anchor.BN(EPOCH_SLOTS)).accounts({ payer: user.publicKey }).rpc()
})

test('a deposit through the adapter records the adapter PDA as depositor, not the user', async () => {
  const recipient = Keypair.generate().publicKey
  await viaAdapter(1_000_000n, recipient, { sequence: 0 })
  const record = await bridgeProgram.account.depositRecord.fetch(recordOf(0))
  assert.equal(record.depositor.toBase58(), adapter.toBase58())
  assert.notEqual(record.depositor.toBase58(), user.publicKey.toBase58())
  assert.equal(record.recipient.toBase58(), recipient.toBase58())
  assert.equal(record.mint.toBase58(), mint.toBase58())
  assert.equal(record.amount.toString(), '1000000')
})

test('the tokens end up in the bridge vault and the escrow is empty again', async () => {
  assert.equal((await getAccount(connection, vaultOf(mint))).amount, 1_000_000n)
  assert.equal((await getAccount(connection, escrowOf(mint))).amount, 0n)
  assert.equal((await getAccount(connection, source)).amount, 10_000_000_000n - 1_000_000n)
})

test('a second deposit takes the next sequence and still leaves the escrow empty', async () => {
  await viaAdapter(2_500_000n, Keypair.generate().publicKey, { sequence: 1 })
  const record = await bridgeProgram.account.depositRecord.fetch(recordOf(1))
  assert.equal(record.amount.toString(), '2500000')
  assert.equal((await getAccount(connection, escrowOf(mint))).amount, 0n)
  assert.equal((await bridgeProgram.account.bridge.fetch(bridge)).deposits.toNumber(), 2)
})

test('rejects amount 0', async () => {
  await assert.rejects(viaAdapter(0n, Keypair.generate().publicKey, { sequence: 2 }), /ZeroAmount|greater than zero/)
})

test('rejects the all-zero recipient and the depositing wallet as recipient', async () => {
  await assert.rejects(viaAdapter(1n, PublicKey.default, { sequence: 2 }), /BadRecipient|not a usable/)
  await assert.rejects(viaAdapter(1n, user.publicKey, { sequence: 2 }), /RecipientIsDepositor|fresh address/)
})

test('rejects a source account of another mint', async () => {
  const other = await createMint(connection, user, user.publicKey, null, 6)
  const otherSource = (await getOrCreateAssociatedTokenAccount(connection, user, other, user.publicKey)).address
  await assert.rejects(viaAdapter(1n, Keypair.generate().publicKey, { sequence: 2, from: otherSource }))
})

test('rejects a source account the user does not own', async () => {
  const stranger = Keypair.generate()
  const theirs = (await getOrCreateAssociatedTokenAccount(connection, user, mint, stranger.publicKey)).address
  await assert.rejects(viaAdapter(1n, Keypair.generate().publicKey, { sequence: 2, from: theirs }))
})

test('rejects a stale or wrong record address (sequence mismatch)', async () => {
  await assert.rejects(viaAdapter(1n, Keypair.generate().publicKey, { sequence: 7 }))
})

test('rejects more than the user holds, and a failed deposit changes nothing', async () => {
  const before = await getAccount(connection, source)
  await assert.rejects(viaAdapter(20_000_000_000n, Keypair.generate().publicKey, { sequence: 2 }))
  assert.equal((await getAccount(connection, source)).amount, before.amount)
  assert.equal((await bridgeProgram.account.bridge.fetch(bridge)).deposits.toNumber(), 2)
})

test('the user\'s wallet is not an account of the bridge call: it appears only as the adapter\'s signer', async () => {
  const sigs = await connection.getSignaturesForAddress(recordOf(0), { limit: 1 })
  const tx = await connection.getTransaction(sigs[0].signature, { maxSupportedTransactionVersion: 0, commitment: 'confirmed' })
  // Honest check of what still leaks: the wallet IS in the transaction. See the doc.
  const keys = tx.transaction.message.staticAccountKeys.map((k) => k.toBase58())
  assert.ok(keys.includes(user.publicKey.toBase58()))
})
