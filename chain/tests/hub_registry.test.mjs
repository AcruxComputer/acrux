// hub_registry on the local validator (32-slot epochs). Run through tests/run.mjs.
import assert from 'node:assert/strict'
import { before, test } from 'node:test'
import { Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { devKeypair } from '../scripts/dev-keys.mjs'
import {
  LOCAL_CONFIG, eventAuthority, feeVaultAddress, hubAddress, initializeLocal, membershipAddress,
  programId, registryAddress, registryProgram,
} from '../scripts/registry.mjs'

const program = registryProgram()
const connection = program.provider.connection
const authority = devKeypair('local-authority')
const registrant = Keypair.generate()
const caller = Keypair.generate() // sends release and sweep; has no role
const asRegistrant = registryProgram(connection, registrant)
const asCaller = registryProgram(connection, caller)
const members = Array.from({ length: 5 }, () => Keypair.generate())
const event = { eventAuthority, program: programId }
const DEPOSIT = LAMPORTS_PER_SOL
const FEE = DEPOSIT * 500 / 10_000
const FRA = { x: 4_044_000, y: 613_000, z: 4_878_000 }
const NYC = { x: 1_334_000, y: -4_651_000, z: 4_141_000 }
const TYO = { x: -3_959_000, y: 3_352_000, z: 3_697_000 }

const epochNow = async () => (await connection.getEpochInfo('confirmed')).epoch
async function waitEpoch(target) {
  while ((await epochNow()) < target) await new Promise((r) => setTimeout(r, 400))
}
/** Returns the epoch once it is young enough for a few transactions to land inside it. */
async function epochStart() {
  for (;;) {
    const info = await connection.getEpochInfo('confirmed')
    if (info.slotIndex < 8) return info.epoch
    await new Promise((r) => setTimeout(r, 400))
  }
}
async function fails(promise, code) {
  await assert.rejects(promise, (err) => {
    assert.equal(err?.error?.errorCode?.code, code, String(err))
    return true
  })
}

const register = (code, location) => asRegistrant.methods.registerHub(code, location).accountsStrict({
  registrant: registrant.publicKey, registry: registryAddress, hub: hubAddress(code), feeVault: feeVaultAddress,
  systemProgram: SystemProgram.programId, ...event,
}).rpc()
const join = (identity, code) => program.methods.joinHub().accountsStrict({
  payer: identity.publicKey, identity: identity.publicKey, registry: registryAddress, hub: hubAddress(code),
  membership: membershipAddress(identity.publicKey), systemProgram: SystemProgram.programId, ...event,
}).signers([identity]).rpc()
const leave = (identity, code) => program.methods.leaveHub().accountsStrict({
  identity: identity.publicKey, registry: registryAddress, hub: hubAddress(code),
  membership: membershipAddress(identity.publicKey), ...event,
}).signers([identity]).rpc()
const move = (identity, from, to) => program.methods.moveHub().accountsStrict({
  identity: identity.publicKey, registry: registryAddress, fromHub: hubAddress(from), toHub: hubAddress(to),
  membership: membershipAddress(identity.publicKey), ...event,
}).signers([identity]).rpc()
const release = (code) => asCaller.methods.releaseHub().accountsStrict({
  registry: registryAddress, hub: hubAddress(code), registrant: registrant.publicKey, ...event,
}).rpc()
const hub = (code) => program.account.hub.fetch(hubAddress(code))
const membership = (identity) => program.account.membership.fetch(membershipAddress(identity.publicKey))

let releaseCodeEpoch // the epoch ZZZ was registered in
let joinEpoch

before(async () => {
  const fund = new Transaction()
  for (const key of [registrant, caller, ...members]) {
    fund.add(SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: key.publicKey, lamports: 10 * LAMPORTS_PER_SOL }))
  }
  await sendAndConfirmTransaction(connection, fund, [authority])
})

test('initialize: only the upgrade authority, with the local configuration', async () => {
  const loader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111')
  await fails(asCaller.methods.initialize(LOCAL_CONFIG).accountsStrict({
    authority: caller.publicKey, registry: registryAddress, feeVault: feeVaultAddress, program: programId,
    programData: PublicKey.findProgramAddressSync([programId.toBuffer()], loader)[0], systemProgram: SystemProgram.programId,
  }).rpc(), 'NotUpgradeAuthority')
  const registry = await initializeLocal(program)
  assert.equal(registry.authority.toBase58(), authority.publicKey.toBase58())
  assert.equal(registry.depositLamports.toNumber(), DEPOSIT)
  assert.equal(registry.registrationFeeBps, 500)
  assert.equal(registry.feeVault.toBase58(), feeVaultAddress.toBase58())
})

test('register: the deposit lands in the hub account and the fee in the fee vault', async () => {
  const vaultBefore = await connection.getBalance(feeVaultAddress)
  const signature = await register('FRA', FRA)
  const account = await connection.getAccountInfo(hubAddress('FRA'))
  const rent = await connection.getMinimumBalanceForRentExemption(account.data.length)
  assert.equal(account.lamports, rent + DEPOSIT)
  assert.equal((await connection.getBalance(feeVaultAddress)) - vaultBefore, FEE)

  const tx = await connection.getTransaction(signature, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })
  assert.equal(tx.meta.preBalances[0] - tx.meta.postBalances[0], rent + DEPOSIT + FEE + tx.meta.fee)

  const fra = await hub('FRA')
  assert.equal(Buffer.from(fra.code).toString().replace(/\0+$/, ''), 'FRA')
  assert.deepEqual(fra.location, FRA)
  assert.equal(fra.registrant.toBase58(), registrant.publicKey.toBase58())
  assert.equal(fra.depositLamports.toNumber(), DEPOSIT)
  assert.equal(fra.lastActiveEpoch.toNumber(), fra.createdEpoch.toNumber())

  await fails(register('FR', FRA), 'InvalidCode')
  await fails(register('Fra', FRA), 'InvalidCode')
  await fails(register('FRANKFURT', FRA), 'InvalidCode')
  await fails(register('MOON', { x: 384_400_000, y: 0, z: 0 }), 'InvalidLocation')
  await assert.rejects(register('FRA', FRA)) // the code is taken

  await register('NYC', NYC)
  await register('ZZZ', TYO)
  releaseCodeEpoch = (await hub('ZZZ')).createdEpoch.toNumber()
})

test('join: pending until the next epoch boundary, then in effect', async () => {
  joinEpoch = await epochStart()
  await Promise.all(members.map((m) => join(m, 'FRA')))
  const pending = await hub('FRA')
  assert.equal(pending.countedEpoch.toNumber(), joinEpoch)
  assert.deepEqual([pending.members, pending.joining, pending.leaving], [0, 5, 0])
  const m = await membership(members[4])
  assert.equal(m.hub.toBase58(), hubAddress('FRA').toBase58())
  assert.equal(m.effectiveEpoch.toNumber(), joinEpoch + 1)
  await fails(leave(members[4], 'FRA'), 'MembershipPending')
  await fails(join(members[4], 'NYC'), 'AlreadyMember')

  // The next instruction that touches the hub counts the five members and marks it active.
  await waitEpoch(joinEpoch + 1)
  await move(members[4], 'FRA', 'NYC')
  const fra = await hub('FRA')
  assert.deepEqual([fra.members, fra.joining, fra.leaving], [5, 0, 1])
  assert.equal(fra.lastActiveEpoch.toNumber(), joinEpoch + 1)
  assert.deepEqual([(await hub('NYC')).members, (await hub('NYC')).joining], [0, 1])
})

test('leave and move: next epoch boundary, then the cooldown', async () => {
  const moving = await membership(members[4])
  assert.deepEqual(Object.keys(moving.pending), ['move'])
  assert.equal(moving.pendingHub.toBase58(), hubAddress('NYC').toBase58())
  assert.equal(moving.pendingEpoch.toNumber(), joinEpoch + 2)
  assert.equal(moving.cooldownUntilEpoch.toNumber(), joinEpoch + 3)
  await fails(leave(members[4], 'FRA'), 'ChangePending')

  // The move is in effect; the cooldown still runs for one epoch.
  await waitEpoch(joinEpoch + 2)
  await fails(leave(members[4], 'FRA'), 'WrongHub')
  await fails(leave(members[4], 'NYC'), 'CooldownActive')
  await fails(move(members[4], 'NYC', 'FRA'), 'CooldownActive')

  await waitEpoch(joinEpoch + 3)
  await leave(members[4], 'NYC')
  const leaving = await membership(members[4])
  assert.equal(leaving.hub.toBase58(), hubAddress('NYC').toBase58())
  assert.equal(leaving.effectiveEpoch.toNumber(), joinEpoch + 2)
  assert.deepEqual(Object.keys(leaving.pending), ['leave'])
  assert.equal(leaving.pendingEpoch.toNumber(), joinEpoch + 4)
  assert.equal(leaving.cooldownUntilEpoch.toNumber(), joinEpoch + 5)
  const nyc = await hub('NYC')
  assert.deepEqual([nyc.members, nyc.joining, nyc.leaving], [1, 0, 1])
  await fails(join(members[4], 'FRA'), 'ChangePending')
})

test('sweep: the fee vault pays out to the configured destination', async () => {
  const destination = LOCAL_CONFIG.feeDestination
  const sweep = () => asCaller.methods.sweepFees().accountsStrict({
    registry: registryAddress, feeVault: feeVaultAddress, destination, ...event,
  }).rpc()
  const vault = await connection.getAccountInfo(feeVaultAddress)
  const rent = await connection.getMinimumBalanceForRentExemption(vault.data.length)
  const swept = vault.lamports - rent
  assert.ok(swept >= 3 * FEE)
  const before = await connection.getBalance(destination)
  await sweep()
  assert.equal((await connection.getBalance(destination)) - before, swept)
  assert.equal(await connection.getBalance(feeVaultAddress), rent)
  await fails(sweep(), 'NothingToSweep')
  await assert.rejects(asCaller.methods.sweepFees().accountsStrict({
    registry: registryAddress, feeVault: feeVaultAddress, destination: caller.publicKey, ...event,
  }).rpc())
})

test('release: after the inactivity period the deposit returns and the code is free', async () => {
  const inactivity = LOCAL_CONFIG.inactivityEpochs.toNumber()
  if ((await epochStart()) <= releaseCodeEpoch + inactivity) await fails(release('ZZZ'), 'HubNotInactive')

  await waitEpoch(releaseCodeEpoch + inactivity + 1)
  await fails(release('FRA'), 'HubNotInactive') // four members in effect
  const held = await connection.getBalance(hubAddress('ZZZ'))
  const firstId = (await hub('ZZZ')).id.toNumber()
  const before = await connection.getBalance(registrant.publicKey)
  await release('ZZZ')
  assert.equal((await connection.getBalance(registrant.publicKey)) - before, held)
  assert.ok(held > DEPOSIT)
  assert.equal(await connection.getAccountInfo(hubAddress('ZZZ')), null)

  await register('ZZZ', TYO)
  assert.ok((await hub('ZZZ')).id.toNumber() > firstId)
})
