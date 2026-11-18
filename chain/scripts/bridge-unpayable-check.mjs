// M4 integration check against an existing bridge stand, validator, Acrux net, API and keeper.
// Deposits the stand's admitted SPL mint, changes a recipient ATA's owner, then withdraws to
// that recipient followed by a normal recipient. The normal payout must finalize while the
// first sequence remains unpaid with a visible releaseError. Starts and stops no processes.
// Usage (from backend/chain): node scripts/bridge-unpayable-check.mjs <stand dir>
//   [--solana http://127.0.0.1:4990] [--acrux http://127.0.0.1:4930]
//   [--api http://127.0.0.1:4920] [--amount <base units per withdrawal>]
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import anchor from '@coral-xyz/anchor'
import {
  AuthorityType, TOKEN_PROGRAM_ID, createAssociatedTokenAccountIdempotentInstruction,
  createSetAuthorityInstruction, getAssociatedTokenAddressSync, unpackAccount,
} from '@solana/spl-token'
import { Connection, Keypair, PublicKey, SystemProgram, Transaction } from '@solana/web3.js'
import { bridgeProgram, pdas } from './bridge-relay.mjs'

const out = process.argv[2]
if (!out || out.startsWith('--')) throw new Error('Usage: node scripts/bridge-unpayable-check.mjs <stand dir> [--amount <base units>].')
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
assert.ok(mintAccount?.owner.equals(TOKEN_PROGRAM_ID), 'The stand mint must use classic SPL Token for SetAuthority.')
const minimumAccount = await acrux.getAccountInfo(minimum, 'finalized')
let floor = 0n
if (minimumAccount && !(minimumAccount.owner.equals(SystemProgram.programId) && minimumAccount.data.length === 0)) {
  const discriminator = createHash('sha256').update('account:MinimumWithdrawal').digest().subarray(0, 8)
  assert.ok(minimumAccount.owner.equals(programId) && minimumAccount.data.length === 16 && minimumAccount.data.subarray(0, 8).equals(discriminator), 'Minimum withdrawal account must match this build.')
  floor = minimumAccount.data.readBigUInt64LE(8)
}
const specified = arg('amount')
const amount = specified === undefined ? (floor > 1_000_000n ? floor : 1_000_000n) : BigInt(specified)
assert.ok(amount > 0n && amount >= floor && amount * 2n <= 0xffffffffffffffffn, 'Choose a positive u64 withdrawal amount at least the configured floor, with room for both deposits.')
const owned = await tokenAccount(solana, source)
assert.ok(owned && owned.amount >= amount * 2n, 'Stand user needs enough test tokens for both withdrawals.')
if (await acrux.getBalance(user.publicKey, 'confirmed') === 0) {
  await http('/v1/faucet', { address: user.publicKey.toBase58() })
  await until('Acrux user funding', 30, async () => await acrux.getBalance(user.publicKey, 'confirmed') > 0)
}
const before = (await tokenAccount(acrux, acruxSource))?.amount ?? 0n
const sequence = (await onSolana.account.bridge.fetch(bridge)).deposits
const deposit = await onSolana.methods.deposit(new anchor.BN((amount * 2n).toString()), user.publicKey).accountsStrict({
  depositor: user.publicKey, bridge, mint, allowed: allowed(mint), source,
  vault: pda(Buffer.from('vault'), mint.toBuffer()), record: pda(Buffer.from('deposit'), u64(sequence)),
  tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  eventAuthority: pda(Buffer.from('__event_authority')), program: programId,
}).instruction()
const depositSignature = await send(solana, [deposit])
await until('The deposit credit on Acrux', 120, async () => {
  const account = await tokenAccount(acrux, acruxSource)
  return account && account.amount >= before + amount * 2n
})

// The recipient signs SetAuthority, while the stand user pays. No private keys are saved.
const badRecipient = Keypair.generate(), goodRecipient = Keypair.generate().publicKey
const badDestination = getAssociatedTokenAddressSync(mint, badRecipient.publicKey)
const goodDestination = getAssociatedTokenAddressSync(mint, goodRecipient)
await send(solana, [
  createAssociatedTokenAccountIdempotentInstruction(user.publicKey, badDestination, badRecipient.publicKey, mint, TOKEN_PROGRAM_ID),
  createSetAuthorityInstruction(badDestination, badRecipient.publicKey, AuthorityType.AccountOwner, user.publicKey, [], TOKEN_PROGRAM_ID),
], [user, badRecipient])
assert.ok((await tokenAccount(solana, badDestination)).owner.equals(user.publicKey), 'Recipient ATA ownership must have moved before the burn.')
async function withdraw(recipient) {
  const counter = await onAcrux.account.outbound.fetchNullable(outbound)
  const sequence = counter?.withdrawals ?? new anchor.BN(0)
  const record = pda(Buffer.from('withdrawal'), u64(sequence))
  const instruction = await onAcrux.methods.withdraw(mint, new anchor.BN(amount.toString()), recipient).accountsStrict({
    owner: user.publicKey, mint: bridgedMint, minimum, source: acruxSource, outbound, record,
    tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }).instruction()
  const signature = await send(acrux, [instruction])
  const withdrawal = await onAcrux.account.withdrawal.fetch(record, 'finalized')
  return { sequence: sequence.toNumber(), signature, cblock: Math.floor(withdrawal.slot.toNumber() / 4) }
}
const bad = await withdraw(badRecipient.publicKey)
const good = await withdraw(goodRecipient)
assert.ok(good.sequence > bad.sequence, 'The normal withdrawal must follow the unpayable withdrawal.')
await until('The normal withdrawal payout', 180, async () => {
  const destination = await tokenAccount(solana, goodDestination)
  return destination?.amount === amount && await isReleased(good.sequence)
})
assert.equal(await isReleased(bad.sequence), false, 'The unpayable withdrawal must retain its unset replay bit.')
const evidence = await until('API payout and failure state', 180, async () => {
  const [view, failures, payouts] = await Promise.all([
    http('/v1/bridge'),
    http(`/v1/bridge/transfers?kind=withdrawal&address=${badRecipient.publicKey.toBase58()}`),
    http(`/v1/bridge/transfers?kind=withdrawal&address=${goodRecipient.toBase58()}`),
  ])
  const failed = failures.items.find(row => row.sequence === bad.sequence)
  const paid = payouts.items.find(row => row.sequence === good.sequence)
  return view.relayer?.unpaid >= 1 && failed?.releaseError && failed.status !== 'released' && paid?.status === 'released'
    ? { unpaid: view.relayer.unpaid, releaseError: failed.releaseError } : null
})
console.log(JSON.stringify({
  event: 'bridge_unpayable_check_passed', deposit: depositSignature, amount: amount.toString(),
  failed: bad, paid: good, ...evidence,
}))
