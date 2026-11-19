// Build 4 step 3 on an operator's fresh local Solana validator at 4990, loaded with
// the rebuilt native bridge. This check never deploys, resets or stops that validator.
// It starts its own Acrux net (TCP 5041..5043, QUIC 5062..5073), uses the checked-in
// Stake placement, and removes its child and unique node/target store on exit.
// ACRUX_IDENTITY_SEED must be a private LOCAL-check seed. All rule values are explicit:
// node scripts/native-stake-check.mjs --native-from-slot 8192 --stake-from-slot 8448 --epoch-slots 256 --native-scale 1000 --genesis-stake-lamports 1000000000
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import dgram from 'node:dgram'
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import net from 'node:net'
import { join, resolve, sep } from 'node:path'
import anchor from '@coral-xyz/anchor'
import {
  MINT_SIZE, TOKEN_PROGRAM_ID, createAssociatedTokenAccountIdempotentInstruction,
  createInitializeMintInstruction, createMintToInstruction, getAssociatedTokenAddressSync,
  unpackAccount,
} from '@solana/spl-token'
import { Authorized, Connection, Keypair, PublicKey, StakeProgram, SystemProgram, Transaction, VoteProgram } from '@solana/web3.js'
import { chainDir, DEV_SEED, devKeypair } from './dev-keys.mjs'
import { bridgeProgram, pdas, seed } from './bridge-relay.mjs'

const arg = name => { const i = process.argv.indexOf(`--${name}`); return i < 0 ? undefined : process.argv[i + 1] }
const required = name => {
  const value = arg(name)
  assert.ok(value && /^\d+$/.test(value), `Set --${name} explicitly as an unsigned decimal integer; see the usage example.`)
  return BigInt(value)
}
const asNumber = (value, name) => {
  assert.ok(value > 0n && value <= BigInt(Number.MAX_SAFE_INTEGER), `${name} must be a positive exactly representable local-check integer.`)
  return Number(value)
}
const nativeFromSlot = asNumber(required('native-from-slot'), 'native-from-slot')
const stakeFromSlot = asNumber(required('stake-from-slot'), 'stake-from-slot')
const epochSlots = asNumber(required('epoch-slots'), 'epoch-slots')
const scale = required('native-scale'), genesisStakeLamports = required('genesis-stake-lamports')
asNumber(scale, 'native-scale')
assert.ok(epochSlots % 16 === 0, 'The local bridge epoch must contain whole sixteen-slot proposer windows.')
assert.ok(nativeFromSlot % epochSlots === 0 && stakeFromSlot % epochSlots === 0 && stakeFromSlot > nativeFromSlot, 'Use separate native and stake activation boundaries in the local bridge epochs.')
const stakeLamports = genesisStakeLamports * 2n
asNumber(stakeLamports, 'twice genesis-stake-lamports')
// Four times the delegation leaves this local holder room for account rent and fees.
// The real Stake program decides whether the requested delegation meets its minimum.
const depositAmount = (stakeLamports * 4n + scale - 1n) / scale
asNumber(depositAmount * scale, 'credited native lamports')
if (!process.env.ACRUX_IDENTITY_SEED?.trim() || process.env.ACRUX_IDENTITY_SEED.trim() === '20261116') throw new Error('Set a private local-check ACRUX_IDENTITY_SEED before running this check.')
const nodeDir = resolve(chainDir, '../node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const solanaUrl = 'http://127.0.0.1:4990', gateway = 'http://127.0.0.1:5041'
const fetchBounded = (input, init = {}) => fetch(input, { ...init, signal: init.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(10_000)]) : AbortSignal.timeout(10_000) })
const connection = url => new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true, fetch: fetchBounded })
const solana = connection(solanaUrl), acrux = connection(gateway)
const authority = devKeypair('local-authority'), holder = Keypair.generate(), mintKeypair = Keypair.generate(), stakeKeypair = Keypair.generate()
const mint = mintKeypair.publicKey, onSolana = bridgeProgram(solana, authority)
const program = onSolana.programId, addresses = pdas(program)
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const u64 = value => { const data = Buffer.alloc(8); data.writeBigUInt64LE(BigInt(value.toString())); return data }
const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
const source = getAssociatedTokenAddressSync(mint, authority.publicKey)
const oldSource = getAssociatedTokenAddressSync(bridgedMint, holder.publicKey)
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const event = (name, fields) => console.log(JSON.stringify({ event: name, ...fields }))
let runDir, child, tail = '', interrupted = false, childError
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function ownNet() {
  if (interrupted) throw new Error('Native stake check interrupted.')
  if (childError) throw childError
  if (child && (child.exitCode !== null || child.signalCode !== null)) throw new Error(`The local net exited: ${tail.slice(-6000)}`)
}
async function until(label, seconds, read) {
  const deadline = Date.now() + seconds * 1000
  while (Date.now() < deadline) {
    ownNet()
    const value = await read()
    if (value) return value
    await pause(500)
  }
  throw new Error(`${label}: deadline exceeded after ${seconds}s.`)
}
async function rpc(method, params = {}) {
  const answer = await fetchBounded(gateway, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })
  assert.ok(answer.ok, `${method}: HTTP ${answer.status}`)
  const body = await answer.json()
  assert.ok(!body.error, `${method}: ${JSON.stringify(body.error)}`)
  return body.result
}
async function send(chain, instructions, payer, additional = []) {
  const latest = await chain.getLatestBlockhash('confirmed')
  const transaction = new Transaction({ feePayer: payer.publicKey, ...latest }).add(...instructions)
  const signers = [payer, ...additional].filter((key, i, all) => all.findIndex(other => other.publicKey.equals(key.publicKey)) === i)
  transaction.sign(...signers)
  const signature = await chain.sendRawTransaction(transaction.serialize(), { preflightCommitment: 'confirmed', maxRetries: 0 })
  await until(`Finalizing ${signature}`, 90, async () => {
    const status = (await chain.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
    if (status?.err) throw new Error(`${signature}: ${JSON.stringify(status.err)}`)
    return status?.confirmationStatus === 'finalized'
  })
  return signature
}
onSolana.provider.sendAndConfirm = (transaction, signers = []) => send(solana, transaction.instructions, authority, signers)
const token = async address => {
  const info = await acrux.getAccountInfo(address, 'finalized')
  return info ? unpackAccount(address, info, TOKEN_PROGRAM_ID) : null
}
async function waitSlot(slot) {
  const now = await acrux.getSlot('finalized')
  return until(`Finalized local slot ${slot}`, Math.ceil(Math.max(0, slot - now) / 20) + 120, async () => {
    const head = await rpc('getFinalizedCblock')
    return head.consensusSlot !== null && head.consensusSlot * 4 >= slot
  })
}
async function stakeAccount() {
  const account = await acrux.getAccountInfo(stakeKeypair.publicKey, 'finalized')
  assert.ok(account && account.owner.equals(StakeProgram.programId) && !account.executable)
  assert.equal(account.data.length, StakeProgram.space)
  assert.equal(account.data.readUInt32LE(0), 2, 'The actual Stake program must write StakeStateV2::Stake.')
  return {
    lamports: account.lamports,
    voter: new PublicKey(account.data.subarray(124, 156)),
    stake: account.data.readBigUInt64LE(156),
    activationEpoch: account.data.readBigUInt64LE(164),
    deactivationEpoch: account.data.readBigUInt64LE(172),
  }
}
async function freePorts() {
  for (const port of [5041, 5042, 5043]) {
    const server = net.createServer()
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen({ port, host: '0.0.0.0', exclusive: true }, resolve) })
    await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()))
  }
  for (let port = 5062; port <= 5073; port++) {
    const socket = dgram.createSocket('udp4')
    try { await new Promise((resolve, reject) => { socket.once('error', reject); socket.bind({ port, address: '0.0.0.0', exclusive: true }, resolve) }) }
    finally { try { socket.close() } catch (error) { if (error.code !== 'ERR_SOCKET_DGRAM_NOT_RUNNING') throw error } }
  }
}

try {
  await freePorts()
  assert.ok(await solana.getAccountInfo(program, 'finalized'), 'Load the rebuilt native bridge on the local validator first.')
  assert.equal(await solana.getAccountInfo(addresses.bridge, 'finalized'), null, 'Use a fresh local validator; this check never resets it.')
  const mintRent = await solana.getMinimumBalanceForRentExemption(MINT_SIZE)
  await send(solana, [
    SystemProgram.createAccount({ fromPubkey: authority.publicKey, newAccountPubkey: mint, lamports: mintRent, space: MINT_SIZE, programId: TOKEN_PROGRAM_ID }),
    createInitializeMintInstruction(mint, 6, authority.publicKey, null, TOKEN_PROGRAM_ID),
    createAssociatedTokenAccountIdempotentInstruction(authority.publicKey, source, authority.publicKey, mint, TOKEN_PROGRAM_ID),
    createMintToInstruction(mint, source, authority.publicKey, depositAmount, [], TOKEN_PROGRAM_ID),
  ], authority, [mintKeypair])
  runDir = mkdtempSync(join(target, 'native-stake-check-'))
  const rule = { program: program.toBase58(), fromSlot: 0, epochSlots, chainId: 'acrux-native-stake-local-check', nativeFromSlot, nativeMint: mint.toBase58(), nativeScale: Number(scale), stakeFromSlot, genesisStakeLamports: genesisStakeLamports.toString() }
  writeFileSync(join(runDir, 'bridge.json'), JSON.stringify(rule))
  mkdirSync(join(runDir, 'placements'))
  const env = { ...process.env, ACRUX_GENESIS_SEED: DEV_SEED, ACRUX_BIND: '127.0.0.1', ACRUX_BRIDGE: join(runDir, 'bridge.json'), ACRUX_BRIDGE_RPC: `${solanaUrl},${solanaUrl}`, ACRUX_BRIDGE_PROOF_MAX: '100000', ACRUX_PLACEMENTS: join(runDir, 'placements/placements.json') }
  for (const name of ['ACRUX_STATE_WRITE_DELAY_MS', 'ACRUX_STATE_WRITE_FAIL_MS', 'ACRUX_LOG_RPC']) delete env[name]
  const oldPlacementBytes = readFileSync(join(chainDir, 'placements/acrux-bridge.json'))
  const oldPlacement = JSON.parse(oldPlacementBytes)
  assert.equal(oldPlacement.accounts.at(-1).address, program.toBase58())
  writeFileSync(join(runDir, 'placements/acrux-bridge-before.json'), oldPlacementBytes)
  const replacementFile = join(runDir, 'placements/acrux-bridge-native.json')
  const replacementRun = spawnSync(process.execPath, ['scripts/place-bridge.mjs', replacementFile], { cwd: chainDir, env, windowsHide: true, encoding: 'utf8', timeout: 30_000 })
  assert.equal(replacementRun.status, 0, 'Prepare the local native placement from the operator-built bridge .so.')
  const replacement = JSON.parse(readFileSync(replacementFile, 'utf8'))
  assert.notEqual(replacement.accounts[0].data, oldPlacement.accounts[0].data, 'The native cut-over must replace the old bytecode.')
  const stakePlacementBytes = readFileSync(join(chainDir, 'placements/stake.json'))
  const stakePlacement = JSON.parse(stakePlacementBytes)
  assert.equal(stakePlacement.accounts.at(-1).address, StakeProgram.programId.toBase58())
  writeFileSync(join(runDir, 'placements/stake.json'), stakePlacementBytes)
  writeFileSync(join(runDir, 'placements/placements.json'), JSON.stringify({ placements: [
    { file: 'acrux-bridge-before.json', slot: 0 },
    { file: 'acrux-bridge-native.json', slot: nativeFromSlot },
    { file: 'stake.json', slot: stakeFromSlot },
  ] }))
  mkdirSync(join(runDir, 'genesis'))
  const genesis = join(runDir, 'genesis/acrux-net.json')
  const generated = spawnSync(process.execPath, ['scripts/genesis.mjs', genesis], { cwd: chainDir, env, windowsHide: true, encoding: 'utf8', timeout: 30_000 })
  assert.equal(generated.status, 0, 'Prepare a local genesis matching the local identity seed.')
  const state = JSON.parse(readFileSync(genesis, 'utf8'))
  for (const entry of state.programs) entry.path = resolve(chainDir, entry.path)
  writeFileSync(genesis, JSON.stringify(state))
  child = spawn(binary, ['net', 'up', '--genesis', genesis, '--store', join(runDir, 'net'), '--gateway-port', '5041', '--ws-port', '5042', '--metrics-port', '5043', '--base-port', '5062', '--traffic-rate', '0'], { cwd: nodeDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  child.once('error', error => { childError = error })
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-64_000) })
  await until('The local net', 60, async () => {
    try { return (await rpc('getFinalizedCblock')).consensusSlot !== null }
    catch (error) { if (error.cause?.code === 'ECONNREFUSED') return false; throw error }
  })
  const first = await rpc('getCblockProof', { latest: true })
  assert.ok(first.consensusSlot * 4 < nativeFromSlot, 'Choose a later local native cut-over slot.')
  await seed(onSolana, first, epochSlots)
  await send(solana, [await onSolana.methods.allowMint().accountsStrict({ authority: authority.publicKey, bridge: addresses.bridge, mint, allowed: addresses.allowed(mint), systemProgram: SystemProgram.programId }).instruction()], authority)
  const sequence = (await onSolana.account.bridge.fetch(addresses.bridge)).deposits
  const deposited = await send(solana, [await onSolana.methods.deposit(new anchor.BN(depositAmount.toString()), holder.publicKey).accountsStrict({
    depositor: authority.publicKey, bridge: addresses.bridge, mint, allowed: addresses.allowed(mint), source,
    vault: pda(Buffer.from('vault'), mint.toBuffer()), record: pda(Buffer.from('deposit'), u64(sequence)),
    tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    eventAuthority: pda(Buffer.from('__event_authority')), program,
  }).instruction()], authority)
  await until('The pre-cut-over bridged balance', 90, async () => (await token(oldSource))?.amount === depositAmount)
  assert.equal(await rpc('getNativeSupply'), null)
  event('native_stake_deposited', { deposited, mint: mint.toBase58(), bridgedBaseUnits: depositAmount.toString(), nativeFromSlot, stakeFromSlot, epochSlots })
  await waitSlot(nativeFromSlot + 4)
  const supply = await rpc('getNativeSupply')
  assert.equal(BigInt(supply.creditedLamports), depositAmount * scale)
  assert.equal(BigInt(await acrux.getBalance(holder.publicKey, 'finalized')), depositAmount * scale)
  assert.equal((await token(oldSource)).amount, 0n)
  await waitSlot(stakeFromSlot + 4)
  assert.ok((await acrux.getAccountInfo(StakeProgram.programId, 'finalized'))?.executable, 'The named Stake placement must be active.')
  const epochInfo = await acrux.getEpochInfo('finalized')
  assert.equal(epochInfo.slotsInEpoch, epochSlots, 'The Stake program and gateway must use the bridge epoch length.')
  const baselineVotes = await acrux.getVoteAccounts('finalized')
  assert.equal(baselineVotes.current.length, 12)
  for (const entry of baselineVotes.current) assert.equal(BigInt(entry.activatedStake), genesisStakeLamports)
  const identity = new PublicKey(baselineVotes.current[0].nodePubkey)
  const vote = PublicKey.findProgramAddressSync([Buffer.from('acrux_genesis_vote'), identity.toBuffer()], VoteProgram.programId)[0]
  assert.equal(baselineVotes.current[0].votePubkey, vote.toBase58())
  assert.ok((await acrux.getAccountInfo(vote, 'finalized'))?.owner.equals(VoteProgram.programId), 'The delegate instruction needs a real genesis Vote account.')
  const baselineEpoch = await rpc('getProposerEpoch', { epoch: epochInfo.epoch })
  const hubCode = baselineEpoch.schedule.windows.find(window => window.proposers.includes(identity.toBase58()))?.hubCode
  assert.ok(hubCode, 'The baseline schedule must contain the genesis validator.')
  const baselineHub = baselineEpoch.schedule.shares.find(share => share.hubCode === hubCode)
  assert.ok(baselineHub)
  const minimumDelegation = await acrux.getStakeMinimumDelegation({ commitment: 'finalized' })
  assert.ok(stakeLamports >= BigInt(minimumDelegation.value), `Twice --genesis-stake-lamports must meet the actual Stake program minimum of ${minimumDelegation.value} native lamports.`)
  event('native_stake_baseline', { proposerEpoch: baselineEpoch, voteAccounts: baselineVotes, minimumDelegationLamports: minimumDelegation.value })
  const rent = await acrux.getMinimumBalanceForRentExemption(StakeProgram.space)
  const accountLamports = asNumber(stakeLamports + BigInt(rent), 'stake account funding')
  assert.ok(BigInt(await acrux.getBalance(holder.publicKey, 'finalized')) > BigInt(accountLamports), 'The converted deposit must cover the delegation, its rent and fees.')
  const create = StakeProgram.createAccount({ fromPubkey: holder.publicKey, stakePubkey: stakeKeypair.publicKey, authorized: new Authorized(holder.publicKey, holder.publicKey), lamports: accountLamports })
  const delegate = StakeProgram.delegate({ stakePubkey: stakeKeypair.publicKey, authorizedPubkey: holder.publicKey, votePubkey: vote })
  const delegated = await send(acrux, [...create.instructions, ...delegate.instructions], holder, [stakeKeypair])
  const initialStake = await stakeAccount()
  assert.ok(initialStake.voter.equals(vote))
  assert.equal(initialStake.stake, stakeLamports)
  assert.equal(initialStake.lamports, accountLamports)
  assert.equal(initialStake.deactivationEpoch, (1n << 64n) - 1n)
  const targetEpoch = asNumber(initialStake.activationEpoch + 2n, 'effective proposer epoch')
  event('native_stake_delegated', { delegated, identity: identity.toBase58(), vote: vote.toBase58(), stakeAccount: stakeKeypair.publicKey.toBase58(), stakeLamports: stakeLamports.toString(), activationEpoch: initialStake.activationEpoch.toString(), targetEpoch })
  await waitSlot(targetEpoch * epochSlots + 4)
  const proposerEpoch = await rpc('getProposerEpoch', { epoch: targetEpoch })
  assert.equal(proposerEpoch.epoch, targetEpoch)
  const expectedWeight = genesisStakeLamports + stakeLamports
  assert.equal(BigInt(proposerEpoch.weights.find(entry => entry.identity === identity.toBase58()).stake), expectedWeight)
  assert.equal(proposerEpoch.schedule.windows.length, 96)
  assert.ok(proposerEpoch.schedule.windows.some(window => window.proposers.includes(identity.toBase58())), 'The actual weighted schedule must draw the delegated validator.')
  const weightedHub = proposerEpoch.schedule.shares.find(share => share.hubCode === hubCode)
  assert.ok(weightedHub)
  assert.ok(BigInt(weightedHub.proposerStake) > BigInt(baselineHub.proposerStake), "The actual schedule must use more capped proposer stake for the delegated validator's hub than the baseline schedule.")
  const weightedVotes = await acrux.getVoteAccounts('finalized')
  assert.equal(BigInt(weightedVotes.current.find(entry => entry.nodePubkey === identity.toBase58()).activatedStake), expectedWeight)
  const afterEpochs = await stakeAccount()
  assert.equal(afterEpochs.lamports, initialStake.lamports, 'Crossing epochs must not issue staking rewards.')
  assert.equal(afterEpochs.stake, initialStake.stake)
  event('native_stake_weighted', { proposerEpoch, voteAccounts: weightedVotes, hubCode, baselineProposerStake: baselineHub.proposerStake, weightedProposerStake: weightedHub.proposerStake, upcomingWindows: await rpc('getSchedule', { limit: 8 }) })
  const deactivated = await send(acrux, StakeProgram.deactivate({ stakePubkey: stakeKeypair.publicKey, authorizedPubkey: holder.publicKey }).instructions, holder)
  const cooling = await stakeAccount()
  assert.ok(cooling.deactivationEpoch < (1n << 64n) - 1n)
  await waitSlot(asNumber(cooling.deactivationEpoch + 1n, 'withdrawable epoch') * epochSlots + 4)
  const inactiveVotes = await acrux.getVoteAccounts('finalized')
  assert.equal(BigInt(inactiveVotes.current.find(entry => entry.nodePubkey === identity.toBase58()).activatedStake), genesisStakeLamports, 'The deactivated delegation must leave the live vote-account weight after cooldown.')
  event('native_stake_inactive', { deactivationEpoch: cooling.deactivationEpoch.toString(), voteAccounts: inactiveVotes })
  const withdrawable = await acrux.getBalance(stakeKeypair.publicKey, 'finalized')
  assert.equal(withdrawable, accountLamports)
  const withdrawn = await send(acrux, StakeProgram.withdraw({ stakePubkey: stakeKeypair.publicKey, authorizedPubkey: holder.publicKey, toPubkey: holder.publicKey, lamports: withdrawable }).instructions, holder)
  assert.equal(await acrux.getAccountInfo(stakeKeypair.publicKey, 'finalized'), null)
  const receipt = await acrux.getTransaction(withdrawn, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
  assert.ok(receipt?.meta && !receipt.meta.err)
  const keys = receipt.transaction.message.accountKeys
  const stakeIndex = keys.findIndex(key => key.equals(stakeKeypair.publicKey)), ownerIndex = keys.findIndex(key => key.equals(holder.publicKey))
  assert.equal(receipt.meta.preBalances[stakeIndex], accountLamports)
  assert.equal(receipt.meta.postBalances[stakeIndex], 0)
  assert.ok(receipt.meta.postBalances[ownerIndex] > receipt.meta.preBalances[ownerIndex])
  event('native_stake_check_passed', { deposited, delegated, deactivated, withdrawn, nativeFromSlot, stakeFromSlot, epochSlots, identity: identity.toBase58(), vote: vote.toBase58(), genesisStakeLamports: genesisStakeLamports.toString(), delegatedLamports: stakeLamports.toString(), activationEpoch: initialStake.activationEpoch.toString(), effectiveEpoch: targetEpoch, deactivationEpoch: cooling.deactivationEpoch.toString(), returnedLamports: withdrawable.toString(), rewardsLamports: '0' })
} catch (error) {
  console.error(`The local net's last output:\n${tail}`)
  throw error
} finally {
  if (child?.pid && child.exitCode === null && child.signalCode === null) {
    const exited = new Promise(resolve => child.once('exit', resolve))
    child.kill('SIGTERM'); await Promise.race([exited, pause(5000)])
    if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await Promise.race([exited, pause(5000)]) }
    assert.ok(child.exitCode !== null || child.signalCode !== null, 'The check-owned net did not stop.')
  }
  if (runDir) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must stay inside node/target.')
    rmSync(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
