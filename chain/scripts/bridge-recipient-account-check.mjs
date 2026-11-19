// M6 integration check against an existing bridge stand, validator, Acrux net, API and keeper.
// A fresh recipient's missing ATA must park the payout without creating or sending to that ATA.
// The stand user then pays its rent, and the short retry must pay the saved withdrawal.
// Starts and stops no processes, and saves no private keys or runtime files.
// Usage (from backend/chain): node scripts/bridge-recipient-account-check.mjs <stand dir>
//   [--solana http://127.0.0.1:4990] [--acrux http://127.0.0.1:4930]
//   [--api http://127.0.0.1:4920] [--amount <base units per withdrawal>]
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import anchor from '@coral-xyz/anchor'
import {
  TOKEN_PROGRAM_ID, createAssociatedTokenAccountIdempotentInstruction, getAssociatedTokenAddressSync, unpackAccount,
} from '@solana/spl-token'
import { Connection, Keypair, PublicKey, SystemProgram, Transaction } from '@solana/web3.js'
import { bridgeProgram, pdas } from './bridge-relay.mjs'
import { BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING } from '../../contract/dist/index.js'

const out = process.argv[2]
if (!out || out.startsWith('--')) throw new Error('Usage: node scripts/bridge-recipient-account-check.mjs <stand dir> [--amount <base units>].')
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`)
  if (i < 0) return fallback
  if (!process.argv[i + 1] || process.argv[i + 1].startsWith('--')) throw new Error(`--${name} requires a value.`)
  return process.argv[i + 1]
}
const localUrl = value => {
  const url = new URL(value)
  if (url.protocol !== 'http:' || !['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) throw new Error('This check accepts local HTTP endpoints only.')
  return url.href.replace(/\/$/, '')
}
const solanaUrl = localUrl(arg('solana', 'http://127.0.0.1:4990'))
const acruxUrl = localUrl(arg('acrux', 'http://127.0.0.1:4930'))
const apiUrl = localUrl(arg('api', 'http://127.0.0.1:4920'))
const boundedFetch = (input, init = {}) => fetch(input, {
  ...init, signal: init.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(10_000)]) : AbortSignal.timeout(10_000),
})
const connection = url => new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true, fetch: boundedFetch })
const solana = connection(solanaUrl), acrux = connection(acruxUrl)
const stand = JSON.parse(readFileSync(join(out, 'stand.json'), 'utf8'))
const user = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(join(out, 'user-keypair.json'), 'utf8'))))
assert.equal(user.publicKey.toBase58(), stand.user, 'Stand user and wallet must match.')
const programId = new PublicKey(stand.program), mint = new PublicKey(stand.mint)
const onSolana = bridgeProgram(solana, user), onAcrux = bridgeProgram(acrux, user)
assert.ok(onSolana.programId.equals(programId), 'Build IDL and stand program must match.')
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, programId)[0]
const u64 = number => { const data = Buffer.alloc(8); data.writeBigUInt64LE(BigInt(number.toString())); return data }
const { bridge, released, allowed } = pdas(programId)
const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
const minimum = pda(Buffer.from('minimum'), mint.toBuffer())
const source = getAssociatedTokenAddressSync(mint, user.publicKey, false, TOKEN_PROGRAM_ID)
const acruxSource = getAssociatedTokenAddressSync(bridgedMint, user.publicKey, false, TOKEN_PROGRAM_ID)
const outbound = pda(Buffer.from('outbound'))
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
async function until(label, seconds, read) {
  const deadline = Date.now() + seconds * 1000
  while (Date.now() < deadline) {
    const value = await read()
    if (value) return value
    await pause(500)
  }
  throw new Error(`${label}: deadline exceeded after ${seconds}s.`)
}
async function http(path, body) {
  const response = await boundedFetch(`${apiUrl}${path}`, body === undefined ? {} : {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  })
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status}.`)
  return response.json()
}
async function send(chain, instructions, signers = [user]) {
  const latest = await chain.getLatestBlockhash('confirmed')
  const transaction = new Transaction({ feePayer: user.publicKey, ...latest }).add(...instructions)
  transaction.sign(...signers)
  const signature = await chain.sendRawTransaction(transaction.serialize(), { preflightCommitment: 'confirmed', maxRetries: 0 })
  await until(`Finalizing ${signature}`, 90, async () => {
    const status = (await chain.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
    if (status?.err) throw new Error(`${signature} failed: ${JSON.stringify(status.err)}.`)
    return status?.confirmationStatus === 'finalized'
  })
  return signature
}
const tokenAccount = async (chain, address) => {
  const account = await chain.getAccountInfo(address, 'finalized')
  return account ? unpackAccount(address, account, TOKEN_PROGRAM_ID) : null
}
async function isReleased(sequence) {
  const account = await solana.getAccountInfo(released(sequence), 'finalized')
  if (!account) return false
  const discriminator = createHash('sha256').update('account:Released').digest().subarray(0, 8)
  assert.ok(account.owner.equals(programId) && account.data.length === 1040 && account.data.subarray(0, 8).equals(discriminator), 'Release bitmap layout must match this build.')
  assert.equal(account.data.readBigUInt64LE(8), BigInt(sequence.toString()) / 8192n)
  const bit = Number(BigInt(sequence.toString()) % 8192n)
  return !!(account.data[16 + Math.floor(bit / 8)] & (1 << (bit % 8)))
}

await until('The keeper reporting this stand', 60, async () => {
  const view = await http('/v1/bridge')
  assert.equal(view.program, stand.program, 'API must read this stand.')
  return view.relayer?.wallet === stand.relayer
})
const mintAccount = await solana.getAccountInfo(mint, 'finalized')
assert.ok(mintAccount?.owner.equals(TOKEN_PROGRAM_ID), 'The stand mint must use classic SPL Token.')
const minimumAccount = await acrux.getAccountInfo(minimum, 'finalized')
let floor = 0n
if (minimumAccount && !(minimumAccount.owner.equals(SystemProgram.programId) && minimumAccount.data.length === 0)) {
  const discriminator = createHash('sha256').update('account:MinimumWithdrawal').digest().subarray(0, 8)
  assert.ok(minimumAccount.owner.equals(programId) && minimumAccount.data.length === 16 && minimumAccount.data.subarray(0, 8).equals(discriminator), 'Minimum withdrawal account must match this build.')
  floor = minimumAccount.data.readBigUInt64LE(8)
}
const specified = arg('amount')
const amount = specified === undefined ? (floor > 1_000_000n ? floor : 1_000_000n) : BigInt(specified)
assert.ok(amount > 0n && amount >= floor && amount <= 0xffffffffffffffffn, 'Choose a positive u64 withdrawal amount at least the configured floor, within u64.')
const owned = await tokenAccount(solana, source)
assert.ok(owned && owned.amount >= amount, 'Stand user needs enough test tokens for the withdrawal.')
if (await acrux.getBalance(user.publicKey, 'confirmed') === 0) {
  await http('/v1/faucet', { address: user.publicKey.toBase58() })
  await until('Acrux user funding', 30, async () => await acrux.getBalance(user.publicKey, 'confirmed') > 0)
}
const before = (await tokenAccount(acrux, acruxSource))?.amount ?? 0n
const sequence = (await onSolana.account.bridge.fetch(bridge)).deposits
const deposit = await onSolana.methods.deposit(new anchor.BN(amount.toString()), user.publicKey).accountsStrict({
  depositor: user.publicKey, bridge, mint, allowed: allowed(mint), source,
  vault: pda(Buffer.from('vault'), mint.toBuffer()), record: pda(Buffer.from('deposit'), u64(sequence)),
  tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  eventAuthority: pda(Buffer.from('__event_authority')), program: programId,
}).instruction()
const depositSignature = await send(solana, [deposit])
await until('The deposit credit on Acrux', 120, async () => {
  const account = await tokenAccount(acrux, acruxSource)
  return account && account.amount >= before + amount
})

// A fresh destination has no history or account rent for the relayer to spend.
const recipient = Keypair.generate().publicKey
const destination = getAssociatedTokenAddressSync(mint, recipient)
assert.equal(await solana.getAccountInfo(destination, 'finalized'), null)
assert.deepEqual(await solana.getSignaturesForAddress(destination, { limit: 100 }, 'finalized'), [])
const vault = pda(Buffer.from('vault'), mint.toBuffer())
const lockedBefore = (await tokenAccount(solana, vault)).amount
const counter = await onAcrux.account.outbound.fetchNullable(outbound)
const withdrawalSequence = counter?.withdrawals ?? new anchor.BN(0)
const record = pda(Buffer.from('withdrawal'), u64(withdrawalSequence))
const instruction = await onAcrux.methods.withdraw(mint, new anchor.BN(amount.toString()), recipient).accountsStrict({
  owner: user.publicKey, mint: bridgedMint, minimum, source: acruxSource, outbound, record,
  tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
}).instruction()
const burn = await send(acrux, [instruction])
const sequenceNumber = withdrawalSequence.toNumber()
const transfer = async () => {
  const page = await http(`/v1/bridge/transfers?kind=withdrawal&address=${recipient.toBase58()}`)
  return page.items.find(row => row.sequence === sequenceNumber)
}
await until('The exact missing-recipient-account reason', 180, async () => {
  const row = await transfer()
  return row?.status !== 'released' && row?.releaseError === BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING
})
assert.equal(await solana.getAccountInfo(destination, 'finalized'), null, 'The relayer must leave the missing ATA absent.')
assert.deepEqual(await solana.getSignaturesForAddress(destination, { limit: 100 }, 'finalized'), [], 'No relayer transaction may have sent to the missing ATA.')
assert.equal((await tokenAccount(solana, vault)).amount, lockedBefore, 'The parked withdrawal must leave its tokens in the vault.')
assert.equal(await isReleased(sequenceNumber), false)
console.log(JSON.stringify({ event: 'bridge_recipient_account_parked', sequence: sequenceNumber, releaseError: BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING }))

// The withdrawer pays the recipient's rent, then the same saved withdrawal is retried.
const create = await send(solana, [createAssociatedTokenAccountIdempotentInstruction(user.publicKey, destination, recipient, mint, TOKEN_PROGRAM_ID)])
const receipt = await solana.getTransaction(create, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
assert.ok(receipt?.meta && !receipt.meta.err)
const keys = receipt.transaction.message.accountKeys
assert.ok(keys[0].equals(user.publicKey), 'The stand user must pay the ATA creation fee.')
const destinationIndex = keys.findIndex(key => key.equals(destination))
assert.ok(destinationIndex >= 0)
const rent = receipt.meta.postBalances[destinationIndex] - receipt.meta.preBalances[destinationIndex]
assert.ok(rent > 0)
assert.equal(receipt.meta.preBalances[0] - receipt.meta.postBalances[0], rent + receipt.meta.fee, 'ATA rent and transaction fee must come from the stand user.')
await until('The saved payout after recipient account creation', 180, async () => {
  const account = await tokenAccount(solana, destination)
  return account?.owner.equals(recipient) && account.amount === amount && await isReleased(sequenceNumber)
})
await until('API reporting the successful retry', 180, async () => {
  const row = await transfer()
  return row?.status === 'released' && row.releaseError === null
})
console.log(JSON.stringify({ event: 'bridge_recipient_account_check_passed', deposit: depositSignature, burn, sequence: sequenceNumber, create, userPaidRentLamports: rent, amount: amount.toString() }))
