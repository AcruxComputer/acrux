// acrux_bridge on the local validator: deposits lock tokens in the mint's vault and are recorded
// in sequence for Acrux to credit. Run through tests/run.mjs.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { before, test } from 'node:test'
import anchor from '@coral-xyz/anchor'
import {
  ExtensionType, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, createInitializeMintInstruction, createInitializeTransferFeeConfigInstruction,
  createMint, getAccount, getMintLen, getOrCreateAssociatedTokenAccount, mintTo,
} from '@solana/spl-token'
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { chainDir, devKeypair } from '../scripts/dev-keys.mjs'
import { RPC_URL } from '../scripts/registry.mjs'

const idl = JSON.parse(readFileSync(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8'))
const connection = new Connection(RPC_URL, 'confirmed')
const authority = devKeypair('local-authority')
const depositor = Keypair.generate()
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(depositor), { commitment: 'confirmed' }))
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program.programId)[0]
const bridge = pda(Buffer.from('bridge'))
const vaultOf = (mint) => pda(Buffer.from('vault'), mint.toBuffer())
const recordOf = (sequence) => pda(Buffer.from('deposit'), new anchor.BN(sequence).toArrayLike(Buffer, 'le', 8))
const recipient = Keypair.generate().publicKey // an Acrux address
let mint, source

const deposit = (amount, { mintKey = mint, from = source, tokenProgram = TOKEN_PROGRAM_ID, sequence } = {}) =>
  program.methods.deposit(new anchor.BN(amount.toString()), recipient).accounts({
    depositor: depositor.publicKey, mint: mintKey, source: from, record: recordOf(sequence), tokenProgram,
  }).rpc()

before(async () => {
  await sendAndConfirmTransaction(connection, new Transaction().add(
    SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: depositor.publicKey, lamports: 10 * LAMPORTS_PER_SOL }),
  ), [authority])
  mint = await createMint(connection, depositor, depositor.publicKey, null, 6)
  source = (await getOrCreateAssociatedTokenAccount(connection, depositor, mint, depositor.publicKey)).address
  await mintTo(connection, depositor, mint, source, depositor, 10_000_000_000n)
})

test('initialize: the bridge starts at deposit 0, once', async () => {
  await program.methods.initialize().accounts({ payer: depositor.publicKey }).rpc()
  assert.equal((await program.account.bridge.fetch(bridge)).deposits.toNumber(), 0)
  await assert.rejects(program.methods.initialize().accounts({ payer: depositor.publicKey }).rpc())
})

test('deposit: tokens lock in the mint\'s vault and each deposit is recorded in sequence', async () => {
  await deposit(1_000_000_000n, { sequence: 0 })
  await deposit(500_000_000n, { sequence: 1 })
  const vault = await getAccount(connection, vaultOf(mint))
  assert.equal(vault.amount, 1_500_000_000n)
  assert.equal(vault.owner.toBase58(), vaultOf(mint).toBase58())
  assert.equal((await getAccount(connection, source)).amount, 8_500_000_000n)
  const first = await program.account.depositRecord.fetch(recordOf(0))
  assert.deepEqual(
    [first.sequence.toNumber(), first.depositor.toBase58(), first.recipient.toBase58(), first.mint.toBase58(), first.amount.toString()],
    [0, depositor.publicKey.toBase58(), recipient.toBase58(), mint.toBase58(), '1000000000'],
  )
  assert.ok(first.slot.toNumber() > 0)
  assert.equal((await program.account.depositRecord.fetch(recordOf(1))).amount.toString(), '500000000')
  assert.equal((await program.account.bridge.fetch(bridge)).deposits.toNumber(), 2)
})

test('deposit: a Token-2022 mint with a transfer fee records what the vault received', async () => {
  const mint22 = Keypair.generate()
  const space = getMintLen([ExtensionType.TransferFeeConfig])
  await sendAndConfirmTransaction(connection, new Transaction().add(
    SystemProgram.createAccount({ fromPubkey: depositor.publicKey, newAccountPubkey: mint22.publicKey, space, lamports: await connection.getMinimumBalanceForRentExemption(space), programId: TOKEN_2022_PROGRAM_ID }),
    createInitializeTransferFeeConfigInstruction(mint22.publicKey, depositor.publicKey, depositor.publicKey, 100, 1_000_000_000n, TOKEN_2022_PROGRAM_ID),
    createInitializeMintInstruction(mint22.publicKey, 6, depositor.publicKey, null, TOKEN_2022_PROGRAM_ID),
  ), [depositor, mint22])
  const from = (await getOrCreateAssociatedTokenAccount(connection, depositor, mint22.publicKey, depositor.publicKey, false, 'confirmed', undefined, TOKEN_2022_PROGRAM_ID)).address
  await mintTo(connection, depositor, mint22.publicKey, from, depositor, 1_000_000_000n, [], undefined, TOKEN_2022_PROGRAM_ID)
  await deposit(100_000_000n, { mintKey: mint22.publicKey, from, tokenProgram: TOKEN_2022_PROGRAM_ID, sequence: 2 })
  // A 1% fee on 100 tokens is withheld in the vault's account: the deposit is 99.
  assert.equal((await program.account.depositRecord.fetch(recordOf(2))).amount.toString(), '99000000')
  assert.equal((await getAccount(connection, vaultOf(mint22.publicKey), 'confirmed', TOKEN_2022_PROGRAM_ID)).amount, 99_000_000n)
})

test('deposit: nothing, or someone else\'s tokens, is refused', async () => {
  await assert.rejects(deposit(0n, { sequence: 3 }), (err) => err.error?.errorCode?.code === 'ZeroAmount')
  const other = Keypair.generate()
  const theirs = (await getOrCreateAssociatedTokenAccount(connection, depositor, mint, other.publicKey)).address
  await mintTo(connection, depositor, mint, theirs, depositor, 1_000_000n)
  await assert.rejects(deposit(1_000_000n, { from: theirs, sequence: 3 }))
  assert.equal((await program.account.bridge.fetch(bridge)).deposits.toNumber(), 3)
})
