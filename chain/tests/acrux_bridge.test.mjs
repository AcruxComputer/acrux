// acrux_bridge on the local validator: deposits lock tokens in the mint's vault and are recorded
// in sequence for Acrux to credit, and the light client certifies cblocks signed by Acrux's
// validators and hands their set on. Run through tests/run.mjs.
import assert from 'node:assert/strict'
import { createHash, generateKeyPairSync, sign } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { before, test } from 'node:test'
import anchor from '@coral-xyz/anchor'
import {
  ExtensionType, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, createInitializeMintInstruction, createInitializeTransferFeeConfigInstruction,
  createMint, getAccount, getMintLen, getOrCreateAssociatedTokenAccount, mintTo,
} from '@solana/spl-token'
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SYSVAR_INSTRUCTIONS_PUBKEY, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { chainDir, devKeypair } from '../scripts/dev-keys.mjs'
import { RPC_URL } from '../scripts/registry.mjs'
import { DEFAULT_EPOCH_SLOTS, ed25519Instruction, outboxProof, pdas, voteMessage } from '../scripts/bridge-relay.mjs'
import { createAssociatedTokenAccountIdempotent } from '@solana/spl-token'

const idl = JSON.parse(readFileSync(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8'))
const connection = new Connection(RPC_URL, 'confirmed')
const authority = devKeypair('local-authority')
const depositor = Keypair.generate()
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(depositor), { commitment: 'confirmed' }))
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program.programId)[0]
const bridge = pda(Buffer.from('bridge'))
const chainId = createHash('sha256').update('acrux-bridge-program-regression-chain').digest()
const loader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111')
const programData = PublicKey.findProgramAddressSync([program.programId.toBuffer()], loader)[0]
const allowedOf = (mint) => pda(Buffer.from('allowed'), mint.toBuffer())
const vaultOf = (mint) => pda(Buffer.from('vault'), mint.toBuffer())
const recordOf = (sequence) => pda(Buffer.from('deposit'), new anchor.BN(sequence).toArrayLike(Buffer, 'le', 8))
const recipient = Keypair.generate().publicKey // an Acrux address
let mint, source

const deposit = (amount, { mintKey = mint, from = source, tokenProgram = TOKEN_PROGRAM_ID, sequence } = {}) =>
  program.methods.deposit(new anchor.BN(amount.toString()), recipient).accounts({
    depositor: depositor.publicKey, mint: mintKey, allowed: allowedOf(mintKey), source: from, record: recordOf(sequence), tokenProgram,
  }).rpc()

before(async () => {
  await sendAndConfirmTransaction(connection, new Transaction().add(
    SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: depositor.publicKey, lamports: 10 * LAMPORTS_PER_SOL }),
  ), [authority])
  mint = await createMint(connection, depositor, depositor.publicKey, null, 6)
  source = (await getOrCreateAssociatedTokenAccount(connection, depositor, mint, depositor.publicKey)).address
  await mintTo(connection, depositor, mint, source, depositor, 10_000_000_000n)
})

test('M2 initialize: only the upgrade authority can create the bridge, once', async () => {
  const initialize = (payer) => program.methods.initialize(new anchor.BN(DEFAULT_EPOCH_SLOTS), new anchor.BN(0), [...chainId]).accountsStrict({
    payer: payer.publicKey, bridge, program: program.programId, programData, systemProgram: SystemProgram.programId,
  }).signers(payer === authority ? [authority] : []).rpc()
  await assert.rejects(initialize(depositor), err => err.error?.errorCode?.code === 'NotUpgradeAuthority')
  assert.equal(await connection.getAccountInfo(bridge), null)
  await initialize(authority)
  const state = await program.account.bridge.fetch(bridge)
  assert.equal(state.deposits.toNumber(), 0)
  assert.equal(state.authority.toBase58(), authority.publicKey.toBase58())
  assert.deepEqual(Buffer.from(state.chainId), chainId)
  assert.equal(state.firstEpoch.toNumber(), 0)
  assert.equal(state.seeded, false)
  await assert.rejects(initialize(authority))
})

const allowMint = (mintKey) => program.methods.allowMint().accountsStrict({
  authority: authority.publicKey, bridge, mint: mintKey, allowed: allowedOf(mintKey), systemProgram: SystemProgram.programId,
}).signers([authority]).rpc()

test('mint allowlist: only the authority admits a mint, and deposits require admission', async () => {
  await assert.rejects(deposit(1n, { sequence: 0 }))
  assert.equal((await program.account.bridge.fetch(bridge)).deposits.toNumber(), 0)
  await assert.rejects(program.methods.allowMint().accountsStrict({
    authority: depositor.publicKey, bridge, mint, allowed: allowedOf(mint), systemProgram: SystemProgram.programId,
  }).rpc())
  await allowMint(mint)
  assert.equal((await program.account.allowedMint.fetch(allowedOf(mint))).mint.toBase58(), mint.toBase58())
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
  await allowMint(mint22.publicKey)
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

// The light client, with three validators staking 40, 30 and 30.
const sha256 = (...parts) => createHash('sha256').update(Buffer.concat(parts)).digest()
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b }
const validators = [40, 30, 30].map((stake) => {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519')
  return { key: Buffer.from(publicKey.export({ format: 'jwk' }).x, 'base64url'), privateKey, stake }
})
const asSet = (list) => list.map((v) => ({ key: new PublicKey(v.key), stake: new anchor.BN(v.stake) }))
// bincode OutboxEntry::ValidatorSet for `epoch`.
const setEntry = (epoch, list) => Buffer.concat([Buffer.alloc(4), u64(epoch), u64(list.length), ...list.flatMap((v) => [v.key, u64(v.stake)])])
const { set: setOf, cblock: cblockOf } = pdas(program.programId)
const slot = 7 // a cblock of epoch 0
const outbox = [setEntry(1, validators), Buffer.from('another entry')]
const root = sha256(sha256(outbox[0]), sha256(outbox[1]))
const preimage = Buffer.concat([u64(slot), Buffer.from('the rest of the cblock')])
const hash = sha256(preimage, root)
const vote = (v, kind, { at = slot, on = hash, tag = chainId } = {}) => {
  const message = voteMessage(tag, v, at, on, kind)
  return { key: validators[v].key, signature: sign(null, message, validators[v].privateKey), message }
}
const attest = async (votes, on = hash) => {
  const ix = await program.methods.attestCblock().accountsPartial({ bridge, cblock: cblockOf(on), set: setOf(0), instructions: SYSVAR_INSTRUCTIONS_PUBKEY }).instruction()
  return program.provider.sendAndConfirm(new Transaction().add(ed25519Instruction(votes), ix), [])
}
const handOff = (epoch, list, proof) => program.methods.handOff(new anchor.BN(epoch), asSet(list), proof).accountsPartial({ payer: depositor.publicKey, cblock: cblockOf(hash) }).rpc()

test('M2 validator seed: only the authority seeds the configured first epoch, once', async () => {
  const stranger = Keypair.generate()
  await sendAndConfirmTransaction(connection, new Transaction().add(SystemProgram.transfer({ fromPubkey: depositor.publicKey, toPubkey: stranger.publicKey, lamports: LAMPORTS_PER_SOL })), [depositor])
  await assert.rejects(program.methods.initValidatorSet(new anchor.BN(0), asSet(validators)).accounts({ authority: stranger.publicKey }).signers([stranger]).rpc())
  const seed = (epoch) => program.methods.initValidatorSet(new anchor.BN(epoch), asSet(validators)).accounts({ authority: authority.publicKey, bridge }).signers([authority]).rpc()
  await assert.rejects(seed(9), err => err.error?.errorCode?.code === 'AlreadySeeded')
  await seed(0)
  assert.equal((await program.account.bridge.fetch(bridge)).seeded, true)
  await assert.rejects(seed(0))
  await assert.rejects(seed(9), err => err.error?.errorCode?.code === 'AlreadySeeded')
  assert.equal((await program.account.validatorSet.fetch(setOf(0))).total.toNumber(), 100)
})

test('light client: a cblock posts only with the preimage and root of its hash', async () => {
  const post = (h, r, p) => program.methods.postCblock([...h], [...r], p).accountsPartial({ payer: depositor.publicKey, cblock: cblockOf(h) }).rpc()
  await assert.rejects(post(hash, Buffer.alloc(32), preimage), (err) => err.error?.errorCode?.code === 'BadPreimage')
  await post(hash, root, preimage)
  const cblock = await program.account.cblock.fetch(cblockOf(hash))
  assert.deepEqual([cblock.slot.toNumber(), cblock.epoch.toNumber(), cblock.certified], [slot, 0, false])
})

test("tagged votes: untagged, wrong-chain and invalid votes are refused", async () => {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519')
  const message = voteMessage(chainId, 0, slot, hash, 'finalize')
  const outsider = { key: Buffer.from(publicKey.export({ format: 'jwk' }).x, 'base64url'), signature: sign(null, message, privateKey), message }
  await assert.rejects(attest([outsider]), (err) => /0x1774/.test(String(err)))
  const oldMessage = message.subarray(32)
  const untagged = { key: validators[0].key, signature: sign(null, oldMessage, validators[0].privateKey), message: oldMessage }
  await assert.rejects(attest([untagged]), err => /0x1774/.test(String(err)))
  await assert.rejects(attest([vote(0, 'finalize', { tag: Buffer.alloc(32, 9) })]), err => /0x1774/.test(String(err)))
  await assert.rejects(attest([vote(0, 'finalize', { at: slot + 1 })]), (err) => /0x1774/.test(String(err)))
  await assert.rejects(attest([vote(0, 'finalize', { on: Buffer.alloc(32, 1) })]), (err) => /0x1774/.test(String(err)))
})

test('light client: 60% of the stake in Finalize votes certifies, and hands the next set on', async () => {
  await attest([vote(0, 'finalize'), vote(1, 'notar')])
  assert.equal((await program.account.cblock.fetch(cblockOf(hash))).certified, false)
  await assert.rejects(handOff(1, validators, outboxProof(outbox, 0)), (err) => err.error?.errorCode?.code === 'NotCertified')
  await attest([vote(1, 'finalize')])
  const cblock = await program.account.cblock.fetch(cblockOf(hash))
  assert.deepEqual([cblock.certified, cblock.finalize, cblock.notar], [true, 0b011, 0b010])
  // A set that is not the one in the outbox, or for another epoch, is refused.
  const changed = validators.map((v, i) => ({ ...v, stake: i === 0 ? 90 : v.stake }))
  await assert.rejects(handOff(1, changed, outboxProof(outbox, 0)), (err) => err.error?.errorCode?.code === 'BadProof')
  await assert.rejects(handOff(2, validators, outboxProof(outbox, 0)), (err) => err.error?.errorCode?.code === 'NotCertified')
  await handOff(1, validators, outboxProof(outbox, 0))
  const next = await program.account.validatorSet.fetch(setOf(1))
  assert.deepEqual([next.epoch.toNumber(), next.total.toNumber(), next.validators.length], [1, 100, 3])
})

// A withdrawal burned on Acrux, in the outbox of another cblock of epoch 0.
const payee = Keypair.generate().publicKey
const withdrawalEntry = (sequence, amount, to = payee) => Buffer.concat([Buffer.from([1, 0, 0, 0]), u64(sequence), mint.toBuffer(), to.toBuffer(), u64(amount)])
const releasing = () => {
  const outbox2 = [setEntry(1, validators), withdrawalEntry(5, 400_000_000), withdrawalEntry(6, 100_000_000)]
  const preimage2 = Buffer.concat([u64(slot + 4), Buffer.from('a later cblock')])
  const root2 = sha256(sha256(sha256(outbox2[0]), sha256(outbox2[1])), sha256(outbox2[2]))
  return { outbox2, preimage2, root2, hash2: sha256(preimage2, root2) }
}
const release = async (hash2, outbox2, { sequence = 5, amount = 400_000_000, to = payee, index = 1 } = {}) => {
  const destination = await createAssociatedTokenAccountIdempotent(connection, depositor, mint, to, { commitment: 'confirmed' })
  return program.methods.release(new anchor.BN(sequence), to, new anchor.BN(amount), outboxProof(outbox2, index))
    .accountsPartial({ payer: depositor.publicKey, cblock: cblockOf(hash2), mint, destination, released: pdas(program.programId).released(sequence), tokenProgram: TOKEN_PROGRAM_ID }).rpc()
}

test('release bitmap: a sequence pays once and a neighboring sequence in the same bucket still pays', async () => {
  const { outbox2, preimage2, root2, hash2 } = releasing()
  await program.methods.postCblock([...hash2], [...root2], preimage2).accountsPartial({ payer: depositor.publicKey, cblock: cblockOf(hash2) }).rpc()
  await assert.rejects(release(hash2, outbox2), (err) => err.error?.errorCode?.code === 'NotCertified')
  const signed = (v) => { const message = voteMessage(chainId, v, slot + 4, hash2, 'finalize'); return { key: validators[v].key, signature: sign(null, message, validators[v].privateKey), message } }
  await attest([signed(0), signed(1)], hash2)
  // Another amount, sequence or recipient is not in the outbox.
  await assert.rejects(release(hash2, outbox2, { amount: 500_000_000 }), (err) => err.error?.errorCode?.code === 'BadProof')
  await assert.rejects(release(hash2, outbox2, { sequence: 7 }), (err) => err.error?.errorCode?.code === 'BadProof')
  const vaultBefore = (await getAccount(connection, vaultOf(mint))).amount
  await release(hash2, outbox2)
  const destination = await createAssociatedTokenAccountIdempotent(connection, depositor, mint, payee, { commitment: 'confirmed' })
  assert.equal((await getAccount(connection, destination)).amount, 400_000_000n)
  assert.equal(vaultBefore - (await getAccount(connection, vaultOf(mint))).amount, 400_000_000n)
  const bitmapAddress = pdas(program.programId).released(5)
  const bitmap = await program.account.released.fetch(bitmapAddress)
  assert.equal(bitmap.bucket.toNumber(), 0)
  assert.equal(bitmap.bits[0], 1 << 5)
  await assert.rejects(release(hash2, outbox2), err => err.error?.errorCode?.code === 'AlreadyReleased')
  // The bitmap account already exists, but sequence 6 has its own replay bit.
  await release(hash2, outbox2, { sequence: 6, amount: 100_000_000, index: 2 })
  assert.equal((await program.account.released.fetch(bitmapAddress)).bits[0], (1 << 5) | (1 << 6))
  assert.equal((await getAccount(connection, destination)).amount, 500_000_000n)
  assert.equal(vaultBefore - (await getAccount(connection, vaultOf(mint))).amount, 500_000_000n)
  await assert.rejects(release(hash2, outbox2, { sequence: 6, amount: 100_000_000, index: 2 }), err => err.error?.errorCode?.code === 'AlreadyReleased')
  assert.equal((await getAccount(connection, destination)).amount, 500_000_000n)
})


// Close receipts prove that rent goes back to the account's recorded payer, after the fee.
const checkRentReturned = async (signature, address, payer = depositor.publicKey) => {
  const receipt = await connection.getTransaction(signature, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })
  assert.ok(receipt?.meta)
  const keys = receipt.transaction.message.accountKeys
  const payerIndex = keys.findIndex(k => k.equals(payer))
  const accountIndex = keys.findIndex(k => k.equals(address))
  assert.ok(payerIndex >= 0 && accountIndex >= 0)
  assert.equal(receipt.meta.postBalances[accountIndex], 0)
  const gained = receipt.meta.postBalances[payerIndex] - receipt.meta.preBalances[payerIndex]
  const expected = receipt.meta.preBalances[accountIndex] - (payerIndex === 0 ? receipt.meta.fee : 0)
  // A JSON receipt carries balances as doubles: the local authority's (about 5e17 lamports) is
  // exact only to its double's spacing, so it is compared within that spacing.
  const spacing = Math.max(1, 2 ** (Math.floor(Math.log2(receipt.meta.preBalances[payerIndex])) - 52))
  assert.ok(Math.abs(gained - expected) <= spacing, `payer gained ${gained}, the closed account held ${expected}`)
  assert.equal(await connection.getAccountInfo(address), null)
}

test('close cblock: only its payer recovers rent, and reposting keeps the release replay guard', async () => {
  const { outbox2, preimage2, root2, hash2 } = releasing()
  const cblock = cblockOf(hash2)
  assert.equal((await program.account.cblock.fetch(cblock)).payer.toBase58(), depositor.publicKey.toBase58())
  await assert.rejects(program.methods.closeCblock().accountsStrict({ payer: authority.publicKey, cblock }).signers([authority]).rpc())
  await checkRentReturned(await program.methods.closeCblock().accountsStrict({ payer: depositor.publicKey, cblock }).rpc(), cblock)
  const released = pdas(program.programId).released(5)
  assert.ok(await connection.getAccountInfo(released))

  // A cblock can be certified again from its proof, but a paid sequence cannot be paid again.
  await program.methods.postCblock([...hash2], [...root2], preimage2).accountsPartial({ payer: depositor.publicKey, cblock }).rpc()
  const signed = (v) => {
    const message = voteMessage(chainId, v, slot + 4, hash2, 'finalize')
    return { key: validators[v].key, signature: sign(null, message, validators[v].privateKey), message }
  }
  await attest([signed(0), signed(1)], hash2)
  await assert.rejects(release(hash2, outbox2))
  await checkRentReturned(await program.methods.closeCblock().accountsStrict({ payer: depositor.publicKey, cblock }).rpc(), cblock)
})

test('close validator set: only its payer recovers rent, and the newest two stay open', async () => {
  // Hand epoch 2 on from a cblock in epoch 1, using the bridge's configured epoch length.
  const epochSlots = (await program.account.bridge.fetch(bridge)).epochSlots.toNumber()
  const at = epochSlots / 4 + 1
  const leaves = [setEntry(2, validators)]
  const nextRoot = sha256(leaves[0])
  const nextPreimage = Buffer.concat([u64(at), Buffer.from('the next epoch')])
  const nextHash = sha256(nextPreimage, nextRoot)
  const cblock = cblockOf(nextHash)
  await program.methods.postCblock([...nextHash], [...nextRoot], nextPreimage).accountsPartial({ payer: depositor.publicKey, cblock }).rpc()
  const signed = [0, 1].map(v => {
    const message = voteMessage(chainId, v, at, nextHash, 'finalize')
    return { key: validators[v].key, signature: sign(null, message, validators[v].privateKey), message }
  })
  const attestNext = await program.methods.attestCblock().accountsPartial({ bridge, cblock, set: setOf(1), instructions: SYSVAR_INSTRUCTIONS_PUBKEY }).instruction()
  await program.provider.sendAndConfirm(new Transaction().add(ed25519Instruction(signed), attestNext), [])
  await program.methods.handOff(new anchor.BN(2), asSet(validators), outboxProof(leaves, 0)).accountsPartial({ payer: depositor.publicKey, cblock }).rpc()
  assert.equal((await program.account.validatorSet.fetch(setOf(0))).payer.toBase58(), authority.publicKey.toBase58())
  assert.equal((await program.account.validatorSet.fetch(setOf(2))).payer.toBase58(), depositor.publicKey.toBase58())

  const close = (epoch, newest) => {
    const payer = epoch === 0 ? authority : depositor
    return program.methods.closeValidatorSet().accountsStrict({ payer: payer.publicKey, set: setOf(epoch), newest: setOf(newest) }).signers(payer === authority ? [authority] : []).rpc()
  }
  await assert.rejects(close(0, 1), err => err.error?.errorCode?.code === 'RecentValidatorSet')
  await assert.rejects(close(1, 2), err => err.error?.errorCode?.code === 'RecentValidatorSet')
  await assert.rejects(close(2, 2))
  await assert.rejects(program.methods.closeValidatorSet().accountsStrict({ payer: depositor.publicKey, set: setOf(0), newest: setOf(2) }).rpc())
  await checkRentReturned(await close(0, 2), setOf(0), authority.publicKey)
  // Closing the first set cannot reopen the authority's seeding power.
  await assert.rejects(program.methods.initValidatorSet(new anchor.BN(0), asSet(validators)).accounts({ authority: authority.publicKey, bridge }).signers([authority]).rpc(), err => err.error?.errorCode?.code === 'AlreadySeeded')
  assert.ok(await connection.getAccountInfo(setOf(1)))
  assert.ok(await connection.getAccountInfo(setOf(2)))
  await program.methods.closeCblock().accountsStrict({ payer: depositor.publicKey, cblock }).rpc()
})
