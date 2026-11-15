// demo_pool on the local validator, against the accounts of genesis/acrux-net.json.
// Run through tests/run.mjs.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { before, test } from 'node:test'
import anchor from '@coral-xyz/anchor'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID, createAssociatedTokenAccountIdempotentInstruction, createMint,
  createTransferInstruction, getAccount, getAssociatedTokenAddressSync, getOrCreateAssociatedTokenAccount, mintTo,
} from '@solana/spl-token'
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { chainDir, devKeypair } from '../scripts/dev-keys.mjs'
import { RPC_URL } from '../scripts/registry.mjs'

const genesis = JSON.parse(readFileSync(join(chainDir, 'genesis/acrux-net.json'), 'utf8'))
const idl = JSON.parse(readFileSync(join(chainDir, 'target/idl/demo_pool.json'), 'utf8'))
const connection = new Connection(RPC_URL, 'confirmed')
const authority = devKeypair('local-authority')
const faucet = devKeypair('faucet')
const user = Keypair.generate()
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(user), { commitment: 'confirmed' }))
const key = (s) => new PublicKey(s)
const pool = genesis.pool
const [mintA, mintB] = [key(pool.mintA), key(pool.mintB)]

/** The pool's quote: the fee comes off the input, rounded up, and stays in the pool. */
function quote(reserveIn, reserveOut, feeBps, amountIn) {
  const fee = (amountIn * BigInt(feeBps) + 9999n) / 10000n
  const net = amountIn - fee
  return { fee, amountOut: reserveOut * net / (reserveIn + net) }
}
const amountOf = async (address) => (await getAccount(connection, address)).amount
const swap = (amountIn, minimumOut, aToB) => program.methods
  .swap(new anchor.BN(amountIn.toString()), new anchor.BN(minimumOut.toString()), aToB)
  .accountsStrict({
    user: user.publicKey, pool: key(pool.address), mintA, mintB, vaultA: key(pool.vaultA), vaultB: key(pool.vaultB),
    userA: getAssociatedTokenAddressSync(mintA, user.publicKey), userB: getAssociatedTokenAddressSync(mintB, user.publicKey),
    tokenProgram: TOKEN_PROGRAM_ID, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }).rpc()

before(async () => {
  await sendAndConfirmTransaction(connection, new Transaction().add(
    SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: user.publicKey, lamports: 10 * LAMPORTS_PER_SOL }),
  ), [authority])
})

test('genesis: the listed accounts load and the pool reads as the description says', async () => {
  for (const a of genesis.accounts) {
    const info = await connection.getAccountInfo(key(a.address))
    assert.equal(info.owner.toBase58(), a.owner, a.label)
    assert.equal(info.data.toString('base64'), a.data, a.label)
    assert.ok(info.lamports >= await connection.getMinimumBalanceForRentExemption(info.data.length), a.label)
  }
  const state = await program.account.pool.fetch(key(pool.address))
  assert.equal(state.vaultA.toBase58(), pool.vaultA)
  assert.equal(state.vaultB.toBase58(), pool.vaultB)
  assert.equal(state.feeBps, pool.feeBps)
  assert.deepEqual([await amountOf(key(pool.vaultA)), await amountOf(key(pool.vaultB))], pool.reserves.map(BigInt))
})

test('swap: the output is the quote, the invariant holds, the minimum is enforced', async () => {
  // The faucet key hands the user 5,000 AXA from its genesis token account.
  const userA = getAssociatedTokenAddressSync(mintA, user.publicKey)
  const userB = getAssociatedTokenAddressSync(mintB, user.publicKey)
  await sendAndConfirmTransaction(connection, new Transaction().add(
    createAssociatedTokenAccountIdempotentInstruction(user.publicKey, userA, user.publicKey, mintA),
    createTransferInstruction(key(genesis.faucet.tokenAccounts[0]), userA, faucet.publicKey, 5_000_000_000n),
  ), [user, faucet])

  const [reserveA, reserveB] = pool.reserves.map(BigInt)
  const amountIn = 1_000_000_000n // 1,000 AXA
  const { fee, amountOut } = quote(reserveA, reserveB, pool.feeBps, amountIn)
  assert.equal(fee, 3_000_000n)
  assert.equal(amountOut, 1_992_013_962n)

  await assert.rejects(swap(amountIn, amountOut + 1n, true), (err) => err.error.errorCode.code === 'SlippageExceeded')
  assert.equal(await connection.getAccountInfo(userB), null)
  await swap(amountIn, amountOut, true) // creates the user's AXB account
  assert.equal(await amountOf(userB), amountOut)
  assert.equal(await amountOf(userA), 4_000_000_000n)
  const [afterA, afterB] = [await amountOf(key(pool.vaultA)), await amountOf(key(pool.vaultB))]
  assert.equal(afterA, reserveA + amountIn)
  assert.equal(afterB, reserveB - amountOut)
  assert.ok(afterA * afterB >= reserveA * reserveB)
  // Without the fee the product would stay within one output unit of the invariant.
  assert.ok((reserveA + amountIn - fee) * afterB - reserveA * reserveB < reserveA + amountIn - fee)

  const back = quote(afterB, afterA, pool.feeBps, amountOut)
  await swap(amountOut, back.amountOut, false)
  assert.equal(await amountOf(userA), 4_000_000_000n + back.amountOut)
  assert.ok(back.amountOut < amountIn)
  const [endA, endB] = [await amountOf(key(pool.vaultA)), await amountOf(key(pool.vaultB))]
  assert.ok(endA * endB >= afterA * afterB)
})

test('initialize_pool: creates the pool and funds its vaults', async () => {
  const mints = [await createMint(connection, user, user.publicKey, null, 6), await createMint(connection, user, user.publicKey, null, 6)]
  const sources = []
  for (const mint of mints) {
    const account = await getOrCreateAssociatedTokenAccount(connection, user, mint, user.publicKey)
    await mintTo(connection, user, mint, account.address, user, 1_000_000_000n)
    sources.push(account.address)
  }
  const [address] = PublicKey.findProgramAddressSync([Buffer.from('pool'), mints[0].toBuffer(), mints[1].toBuffer()], program.programId)
  const vault = (mint) => PublicKey.findProgramAddressSync([Buffer.from('vault'), address.toBuffer(), mint.toBuffer()], program.programId)[0]
  await program.methods.initializePool(25, new anchor.BN(400_000_000), new anchor.BN(900_000_000)).accountsStrict({
    payer: user.publicKey, mintA: mints[0], mintB: mints[1], pool: address, vaultA: vault(mints[0]), vaultB: vault(mints[1]),
    payerA: sources[0], payerB: sources[1], tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }).rpc()
  const state = await program.account.pool.fetch(address)
  assert.equal(state.feeBps, 25)
  assert.equal(await amountOf(vault(mints[0])), 400_000_000n)
  assert.equal(await amountOf(vault(mints[1])), 900_000_000n)
  assert.equal((await getAccount(connection, vault(mints[0]))).owner.toBase58(), address.toBase58())
  // The genesis pool account has the shape this instruction writes.
  const [made, loaded] = [await connection.getAccountInfo(address), await connection.getAccountInfo(key(pool.address))]
  assert.equal(loaded.data.length, made.data.length)
  assert.equal(loaded.lamports, made.lamports)
})
