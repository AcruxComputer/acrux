// Build 5 step 5 on an operator's fresh local Solana validator at 4990, loaded with
// the rebuilt native bridge. This check never deploys, resets or stops that validator.
// It starts its own Acrux net (TCP 5041..5043, UDP 5062..5074), four outside SIN nodes
// (peer/RPC/metrics triples 5080..5091) and one RPC follower. Every child and its unique
// node/target store, temporary identities and placements are removed on exit.
// Leaving one member keeps three effective registry memberships and removes SIN's admitted voters.
// ACRUX_IDENTITY_SEED must be a private LOCAL-check seed. All rule values are explicit:
// node scripts/native-hub-check.mjs --native-from-slot 8192 --stake-from-slot 8704 --admission-from-slot 9216 --epoch-slots 512 --native-scale 1000 --genesis-stake-lamports 1000000000
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
import { bridgeProgram, pdas, chainTag, programData } from './bridge-relay.mjs'

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
const admissionFromSlot = asNumber(required('admission-from-slot'), 'admission-from-slot')
const epochSlots = asNumber(required('epoch-slots'), 'epoch-slots')
const scale = required('native-scale'), genesisStakeLamports = required('genesis-stake-lamports')
asNumber(scale, 'native-scale')
assert.ok(epochSlots % 32 === 0, 'The local epoch midpoint must be a sixteen-slot proposer-window boundary.')
assert.ok(nativeFromSlot % epochSlots === 0 && stakeFromSlot % epochSlots === 0 && stakeFromSlot > nativeFromSlot, 'Use separate native and stake activation boundaries in the local bridge epochs.')
assert.ok(admissionFromSlot % epochSlots === 0 && admissionFromSlot > stakeFromSlot, 'Admission must start at a later local epoch boundary than Stake activation.')
const stakeLamports = genesisStakeLamports * 2n
asNumber(stakeLamports, 'twice genesis-stake-lamports')
// Fund four independent operators and retain a fifth allocation for the funder's fees.
// The real Registry and Stake programs determine deposits, rent and minimum delegation.
const operatorFunding = stakeLamports * 4n
const depositAmount = (operatorFunding * 5n + scale - 1n) / scale
asNumber(depositAmount * scale, 'credited native lamports')
if (!process.env.ACRUX_IDENTITY_SEED?.trim() || process.env.ACRUX_IDENTITY_SEED.trim() === '20261116') throw new Error('Set a private local-check ACRUX_IDENTITY_SEED before running this check.')
const nodeDir = resolve(chainDir, '../node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const solanaUrl = 'http://127.0.0.1:4990', gateway = 'http://127.0.0.1:5041'
const fetchBounded = (input, init = {}) => fetch(input, { ...init, signal: init.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(10_000)]) : AbortSignal.timeout(10_000) })
const connection = url => new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true, fetch: fetchBounded })
const solana = connection(solanaUrl), acrux = connection(gateway)
const authority = devKeypair('local-authority'), holder = Keypair.generate(), mintKeypair = Keypair.generate()
const mint = mintKeypair.publicKey, onSolana = bridgeProgram(solana, authority)
const program = onSolana.programId, addresses = pdas(program)
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const u64 = value => { const data = Buffer.alloc(8); data.writeBigUInt64LE(BigInt(value.toString())); return data }
const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
const source = getAssociatedTokenAddressSync(mint, authority.publicKey)
const oldSource = getAssociatedTokenAddressSync(bridgedMint, holder.publicKey)
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const event = (name, fields) => console.log(JSON.stringify({ event: name, ...fields }))
const operators = Array.from({ length: 4 }, (_, index) => ({
  key: Keypair.generate(), stake: Keypair.generate(), peerPort: 5080 + index * 3,
  rpcPort: 5081 + index * 3, metricsPort: 5082 + index * 3,
  child: null, tail: '', line: '', states: new Map(), proposed: new Set(),
}))
const outside = new Set(operators.map(operator => operator.key.publicKey.toBase58()))
let runDir, child, follower, tail = '', followerTail = '', followerLine = '', interrupted = false, childError
const followerStates = new Map()
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function ownNet() {
  if (interrupted) throw new Error('Native hub check interrupted.')
  if (childError) throw childError
  for (const [name, owned, output] of [['local net', child, tail], ['RPC follower', follower, followerTail],
    ...operators.map(operator => [operator.key.publicKey.toBase58(), operator.child, operator.tail])]) {
    if (owned && (owned.exitCode !== null || owned.signalCode !== null)) throw new Error(`${name} exited: ${output.slice(-6000)}`)
  }
}
async function until(label, seconds, read) {
  const deadline = Date.now() + seconds * 1000
  while (Date.now() < deadline) {
    ownNet()
    const value = await read()
    if (value) return value
    await pause(250)
  }
  throw new Error(`${label}: deadline exceeded after ${seconds}s.`)
}
async function rpc(method, params = {}) {
  const answer = await fetchBounded(gateway, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })
  assert.ok(answer.ok, `${method}: HTTP ${answer.status}`)
  const body = await answer.json()
  if (body.error) { const error = new Error(`${method}: ${JSON.stringify(body.error)}`); error.rpcCode = body.error.data?.error?.code ?? body.error.data?.code; throw error }
  return body.result
}
async function send(chain, instructions, payer, additional = [], commitment = 'finalized') {
  const latest = await chain.getLatestBlockhash('confirmed')
  const transaction = new Transaction({ feePayer: payer.publicKey, ...latest }).add(...instructions)
  const signers = [payer, ...additional].filter((key, i, all) => all.findIndex(other => other.publicKey.equals(key.publicKey)) === i)
  transaction.sign(...signers)
  const signature = await chain.sendRawTransaction(transaction.serialize(), { preflightCommitment: 'confirmed', maxRetries: 0 })
  await until(`Finalizing ${signature}`, 90, async () => {
    const status = (await chain.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
    if (status?.err) throw new Error(`${signature}: ${JSON.stringify(status.err)}`)
    return status?.confirmationStatus === 'finalized' || commitment === 'confirmed' && status?.confirmationStatus === 'confirmed'
  })
  return signature
}
onSolana.provider.sendAndConfirm = (transaction, signers = []) => send(solana, transaction.instructions, authority, signers, 'confirmed')
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
async function freePorts() {
  for (const port of [5041, 5042, 5043, ...operators.flatMap(operator => [operator.rpcPort, operator.metricsPort])]) {
    const server = net.createServer()
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen({ port, host: '127.0.0.1', exclusive: true }, resolve) })
    await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()))
  }
  for (const port of [...Array.from({ length: 13 }, (_, index) => 5062 + index), ...operators.map(operator => operator.peerPort)]) {
    const socket = dgram.createSocket('udp4')
    try { await new Promise((resolve, reject) => { socket.once('error', reject); socket.bind({ port, address: '127.0.0.1', exclusive: true }, resolve) }) }
    finally { try { socket.close() } catch (error) { if (error.code !== 'ERR_SOCKET_DGRAM_NOT_RUNNING') throw error } }
  }
}

const hubStatus = (set, code = 'SIN') => set.hubs.find(hub => hub.hubCode === code)
const ecef = (latitude, longitude) => {
  const a = 6_378_137, e2 = 6.69437999014e-3, phi = latitude * Math.PI / 180, lambda = longitude * Math.PI / 180
  const n = a / Math.sqrt(1 - e2 * Math.sin(phi) ** 2)
  return { x: Math.round(n * Math.cos(phi) * Math.cos(lambda)), y: Math.round(n * Math.cos(phi) * Math.sin(lambda)), z: Math.round(n * (1 - e2) * Math.sin(phi)) }
}
async function freshEpoch() {
  const started = await acrux.getSlot('processed'), epoch = Math.floor(started / epochSlots)
  if (started % epochSlots < epochSlots / 4) return epoch
  await waitSlot((epoch + 1) * epochSlots + 4)
  return epoch + 1
}
async function getJson(url) {
  const response = await fetchBounded(url)
  assert.ok(response.ok, `${url}: HTTP ${response.status}`)
  return response.json()
}
function remember(map, slot, value) {
  map.set(slot, value)
  if (map.size > 2400) map.delete(map.keys().next().value)
}
function track(operator, data) {
  operator.tail = (operator.tail + data).slice(-64_000); operator.line += data
  let newline
  while ((newline = operator.line.indexOf('\n')) >= 0) {
    const line = operator.line.slice(0, newline); operator.line = operator.line.slice(newline + 1)
    const matched = /^slot (\d+) re-executed.* state (\S+) matches\b/.exec(line)
    if (matched) remember(operator.states, Number(matched[1]), matched[2])
    else if (/^slot \d+ re-executed\b/.test(line)) childError = new Error(`Outside replay mismatch: ${line}`)
    const proposed = /^slot (\d+) proposed by (\S+)/.exec(line)
    if (proposed) {
      if (proposed[2] !== operator.key.publicKey.toBase58()) childError = new Error(`Proposer process signed as another identity: ${proposed[2]}`)
      operator.proposed.add(Number(proposed[1]))
      if (operator.proposed.size > 2400) operator.proposed.delete(operator.proposed.values().next().value)
    }
  }
}

try {
  await freePorts()
  assert.ok(await solana.getAccountInfo(program, 'finalized'), 'Load the rebuilt native bridge on the local validator first.')
  assert.equal(await solana.getAccountInfo(addresses.bridge, 'finalized'), null, 'Use a fresh local validator; this check never resets it.')
  // Initialize for the admission epoch now; deposits do not require a seeded light client.
  // This avoids relaying every pre-native epoch just to reach the admission under test.
  const chainId = 'acrux-native-hub-local-check'
  await send(solana, [await onSolana.methods.initialize(new anchor.BN(epochSlots), new anchor.BN(admissionFromSlot / epochSlots), [...chainTag(chainId)]).accountsStrict({
    payer: authority.publicKey, bridge: addresses.bridge, program, programData: programData(program), systemProgram: SystemProgram.programId,
  }).instruction()], authority)
  const mintRent = await solana.getMinimumBalanceForRentExemption(MINT_SIZE)
  await send(solana, [
    SystemProgram.createAccount({ fromPubkey: authority.publicKey, newAccountPubkey: mint, lamports: mintRent, space: MINT_SIZE, programId: TOKEN_PROGRAM_ID }),
    createInitializeMintInstruction(mint, 6, authority.publicKey, null, TOKEN_PROGRAM_ID),
    createAssociatedTokenAccountIdempotentInstruction(authority.publicKey, source, authority.publicKey, mint, TOKEN_PROGRAM_ID),
    createMintToInstruction(mint, source, authority.publicKey, depositAmount, [], TOKEN_PROGRAM_ID),
  ], authority, [mintKeypair])
  runDir = mkdtempSync(join(target, 'native-hub-check-'))
  const rule = { program: program.toBase58(), fromSlot: 0, epochSlots, chainId, nativeFromSlot, nativeMint: mint.toBase58(), nativeScale: Number(scale), stakeFromSlot, genesisStakeLamports: genesisStakeLamports.toString(), admissionFromSlot }
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
  child = spawn(binary, ['net', 'up', '--genesis', genesis, '--store', join(runDir, 'net'), '--gateway-port', '5041', '--ws-port', '5042', '--metrics-port', '5043', '--base-port', '5062', '--traffic-rate', '0', '--peer-port', '5074', '--peer-address', '127.0.0.1:5074'], { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  child.once('error', error => { childError = error })
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-64_000) })
  await until('The local net', 60, async () => {
    try { return (await rpc('getFinalizedCblock')).consensusSlot !== null }
    catch (error) { if (error.cause?.code === 'ECONNREFUSED') return false; throw error }
  })
  const first = await rpc('getCblockProof', { latest: true })
  assert.ok(first.consensusSlot * 4 < nativeFromSlot, 'Choose a later local native cut-over slot.')
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
  event('native_hub_deposited', { deposited, mint: mint.toBase58(), bridgedBaseUnits: depositAmount.toString(), nativeFromSlot, stakeFromSlot, epochSlots })
  await waitSlot(nativeFromSlot + 4)
  const supply = await rpc('getNativeSupply')
  assert.equal(BigInt(supply.creditedLamports), depositAmount * scale)
  assert.equal(BigInt(await acrux.getBalance(holder.publicKey, 'finalized')), depositAmount * scale)
  assert.equal((await token(oldSource)).amount, 0n)
  await waitSlot(stakeFromSlot + 4)
  assert.ok((await acrux.getAccountInfo(StakeProgram.programId, 'finalized'))?.executable, 'The named Stake placement must be active.')

  await waitSlot(admissionFromSlot + 4)
  const activationEpoch = admissionFromSlot / epochSlots
  const initial = await rpc('getValidatorSet', { epoch: activationEpoch })
  const genesisIdentities = new Set(initial.members.map(member => member.identity))
  assert.equal(genesisIdentities.size, 12)
  assert.ok([...outside].every(identity => !genesisIdentities.has(identity)))
  await send(acrux, operators.map(operator => SystemProgram.transfer({ fromPubkey: holder.publicKey,
    toPubkey: operator.key.publicKey, lamports: asNumber(operatorFunding, 'operator funding') })), holder)

  const hubIdl = JSON.parse(readFileSync(join(chainDir, 'target/idl/hub_registry.json'), 'utf8'))
  const registry = new anchor.Program(hubIdl, new anchor.AnchorProvider(acrux, new anchor.Wallet(holder), { commitment: 'confirmed' }))
  const rpda = (...seeds) => PublicKey.findProgramAddressSync(seeds, registry.programId)[0]
  const hub = rpda(Buffer.from('hub'), Buffer.from('SIN')), registryAddress = rpda(Buffer.from('registry'))
  const registryEvents = rpda(Buffer.from('__event_authority'))
  const registryConfig = await registry.account.registry.fetch(registryAddress)
  assert.equal(registryConfig.minMembers, 4, 'The local registry must use the product minimum of four members.')
  assert.ok(operatorFunding > stakeLamports + BigInt(registryConfig.depositLamports.toString()))
  assert.equal(await acrux.getAccountInfo(hub, 'finalized'), null, 'SIN must be formed by this check, not exist at genesis.')
  const joinEpoch = await freshEpoch()
  const registered = await send(acrux, [await registry.methods.registerHub('SIN', ecef(1.3644, 103.9915)).accountsStrict({
    registrant: operators[0].key.publicKey, registry: registryAddress, hub,
    feeVault: rpda(Buffer.from('fee_vault')), systemProgram: SystemProgram.programId,
    eventAuthority: registryEvents, program: registry.programId,
  }).instruction()], operators[0].key)
  const hubRecord = await registry.account.hub.fetch(hub)
  assert.ok(hubRecord.registrant.equals(operators[0].key.publicKey))
  for (const operator of operators) {
    operator.membership = rpda(Buffer.from('member'), operator.key.publicKey.toBuffer())
    operator.joined = await send(acrux, [await registry.methods.joinHub().accountsStrict({
      payer: operator.key.publicKey, identity: operator.key.publicKey, registry: registryAddress, hub,
      membership: operator.membership, systemProgram: SystemProgram.programId,
      eventAuthority: registryEvents, program: registry.programId,
    }).instruction()], operator.key)
    const member = await registry.account.membership.fetch(operator.membership)
    operator.effectiveEpoch = member.effectiveEpoch.toNumber()
    assert.equal(operator.effectiveEpoch, joinEpoch + 1, 'All four joins must finalize in one fresh local epoch; increase --epoch-slots on a slow machine.')
    operator.vote = PublicKey.findProgramAddressSync([Buffer.from('acrux_genesis_vote'), operator.key.publicKey.toBuffer()], VoteProgram.programId)[0]
  }
  const effectiveEpoch = joinEpoch + 1
  event('native_hub_registered', { hubCode: 'SIN', hub: hub.toBase58(), registered,
    identities: [...outside], joins: operators.map(operator => operator.joined), effectiveEpoch })
  await waitSlot(effectiveEpoch * epochSlots + 4)
  await until('Every effective SIN member gets a real Vote PDA', 30, async () => {
    const accounts = await acrux.getMultipleAccountsInfo(operators.map(operator => operator.vote), 'finalized')
    return accounts.every(account => account?.owner.equals(VoteProgram.programId) && !account.executable)
  })
  const unstakedHub = hubStatus(await rpc('getValidatorSet'))
  assert.ok(unstakedHub?.formedOutside && !unstakedHub.active)

  for (const operator of operators) {
    const identityFile = join(runDir, `outside-${operator.peerPort}-keypair.json`)
    writeFileSync(identityFile, JSON.stringify([...operator.key.secretKey]), { mode: 0o600 })
    operator.child = spawn(binary, ['node', 'propose', '--hub', 'SIN', '--identity', identityFile, '--net', gateway, '--genesis', genesis,
      '--peer-port', String(operator.peerPort), '--peer-address', `127.0.0.1:${operator.peerPort}`,
      '--rpc-port', String(operator.rpcPort), '--metrics-port', String(operator.metricsPort)],
    { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
    operator.child.once('error', error => { childError = error })
    operator.child.stderr.on('data', data => { operator.tail = (operator.tail + data).slice(-64_000) })
    operator.child.stdout.on('data', data => track(operator, data.toString()))
  }
  await until('All four SIN nodes independently replay before staking', 90, async () => operators.every(operator => operator.states.size >= 8))

  const rent = await acrux.getMinimumBalanceForRentExemption(StakeProgram.space)
  const minimum = await acrux.getStakeMinimumDelegation({ commitment: 'finalized' })
  assert.ok(stakeLamports >= BigInt(minimum.value), 'Twice --genesis-stake-lamports must meet the actual Stake program minimum.')
  const delegatedEpoch = await freshEpoch(), targetEpoch = delegatedEpoch + 1
  for (const operator of operators) {
    const create = StakeProgram.createAccount({ fromPubkey: operator.key.publicKey, stakePubkey: operator.stake.publicKey,
      authorized: new Authorized(operator.key.publicKey, operator.key.publicKey), lamports: asNumber(stakeLamports + BigInt(rent), 'stake funding') })
    const delegate = StakeProgram.delegate({ stakePubkey: operator.stake.publicKey, authorizedPubkey: operator.key.publicKey, votePubkey: operator.vote })
    operator.delegated = await send(acrux, [...create.instructions, ...delegate.instructions], operator.key, [operator.stake])
    const account = await acrux.getAccountInfo(operator.stake.publicKey, 'finalized')
    assert.ok(account?.owner.equals(StakeProgram.programId) && account.data.length === StakeProgram.space)
    assert.equal(account.data.readUInt32LE(0), 2)
    assert.ok(new PublicKey(account.data.subarray(124, 156)).equals(operator.vote))
    assert.equal(account.data.readBigUInt64LE(156), stakeLamports)
    assert.equal(account.data.readBigUInt64LE(164), BigInt(delegatedEpoch))
    const receipt = await acrux.getTransaction(operator.delegated, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
    assert.ok(receipt && receipt.slot < delegatedEpoch * epochSlots + epochSlots / 2,
      'All four delegations must precede the cutoff; increase --epoch-slots on a slow machine.')
  }
  await waitSlot(delegatedEpoch * epochSlots + epochSlots / 2 + 4)
  const frozen = await until('The next finalized set admits the complete SIN hub', 30, async () => {
    try {
      const set = await rpc('getValidatorSet', { epoch: targetEpoch })
      return hubStatus(set)?.active ? set : false
    } catch (error) { if (error.rpcCode === 'NET_UNAVAILABLE') return false; throw error }
  })
  const frozenHub = hubStatus(frozen)
  assert.ok(frozenHub.formedOutside)
  assert.equal(frozenHub.activatedAtEpoch, targetEpoch)
  const sinMembers = frozen.members.filter(member => member.hub === 'SIN')
  assert.deepEqual(new Set(sinMembers.map(member => member.identity)), outside)
  assert.ok(sinMembers.every(member => member.validatorKind === 'external' && BigInt(member.stake) === stakeLamports))
  assert.equal(hubStatus(await rpc('getValidatorSet')).active, false, 'The frozen hub must not become active before its boundary.')
  const scheduled = await rpc('getProposerEpoch', { epoch: targetEpoch })
  const sinWindows = scheduled.schedule.windows.filter(window => window.hubCode === 'SIN')
  assert.ok(sinWindows.length > 0, 'The admitted hub must receive actual proposer windows.')
  for (const window of sinWindows) {
    assert.equal(window.proposers.length, 3)
    assert.equal(new Set(window.proposers).size, 3)
    assert.ok(window.proposers.every(identity => outside.has(identity)))
  }

  const peers = await until('The four operators announce their own real peer endpoints', 60, async () => {
    try {
      const discovered = await rpc('getHubPeers', { hub: 'SIN', direct: true })
      return discovered.peers.length === 4 && discovered.peers.every(peer => peer.address && peer.certificate) ? discovered : false
    } catch (error) { if (error.rpcCode === 'NOT_FOUND') return false; throw error }
  })
  assert.equal(peers.hub, 'SIN')
  assert.deepEqual(new Set(peers.peers.map(peer => peer.identity)), outside)
  for (const peer of peers.peers) {
    const operator = operators.find(operator => operator.key.publicKey.toBase58() === peer.identity)
    assert.equal(peer.address, `127.0.0.1:${operator.peerPort}`)
    assert.ok(Number.isSafeInteger(peer.id) && peer.id >= 12)
    assert.ok(Buffer.from(peer.certificate, 'base64').length > 0)
  }
  event('native_hub_peers_announced', { hubCode: 'SIN', peers: peers.peers.map(peer => ({ identity: peer.identity, address: peer.address })) })


  follower = spawn(binary, ['node', 'follow', gateway, '--genesis', genesis],
    { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  follower.once('error', error => { childError = error })
  follower.stderr.on('data', data => { followerTail = (followerTail + data).slice(-64_000) })
  follower.stdout.on('data', data => {
    followerTail = (followerTail + data).slice(-64_000); followerLine += data
    let newline
    while ((newline = followerLine.indexOf('\n')) >= 0) {
      const line = followerLine.slice(0, newline); followerLine = followerLine.slice(newline + 1)
      const matched = /^slot (\d+) re-executed.* state (\S+) matches\b/.exec(line)
      if (matched) remember(followerStates, Number(matched[1]), matched[2])
      else if (/^slot \d+ re-executed\b/.test(line)) childError = new Error(`RPC follower mismatch: ${line}`)
    }
  })
  await until('The RPC follower independently matches the chain', 60, async () => followerStates.size >= 8)
  await waitSlot(targetEpoch * epochSlots + 4)
  const active = await rpc('getValidatorSet')
  assert.ok(hubStatus(active)?.active)
  assert.deepEqual(new Set(active.members.filter(member => member.hub === 'SIN').map(member => member.identity)), outside)
  const drawn = await until('A live SIN window draws three of the four independent operators', 120, async () => {
    const window = await rpc('getCurrentWindow')
    if (window.hubCode !== 'SIN') return false
    const latest = await acrux.getSlot('processed')
    if (window.lastSlot - latest < 10) return false
    assert.equal(window.proposers.length, 3)
    assert.ok(window.proposers.every(member => member.validatorKind === 'external' && outside.has(member.identity)))
    return window
  })
  const proposer = operators.find(operator => operator.key.publicKey.toBase58() === drawn.proposers[0].identity)
  const recipient = Keypair.generate().publicKey, transferLamports = await acrux.getMinimumBalanceForRentExemption(0)
  const latest = await acrux.getLatestBlockhash('confirmed')
  const transaction = new Transaction({ feePayer: proposer.key.publicKey, ...latest }).add(
    SystemProgram.transfer({ fromPubkey: proposer.key.publicKey, toPubkey: recipient, lamports: transferLamports }))
  transaction.sign(proposer.key)
  const direct = connection(`http://127.0.0.1:${proposer.rpcPort}`)
  const signature = await direct.sendRawTransaction(transaction.serialize(), { skipPreflight: true, maxRetries: 0 })
  await until('The transfer from an actual SIN proposer finalizes', 60, async () => {
    const status = (await acrux.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
    if (status?.err) throw new Error(`SIN transfer failed: ${JSON.stringify(status.err)}`)
    return status?.confirmationStatus === 'finalized'
  })
  const receipt = await acrux.getTransaction(signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
  assert.ok(receipt?.meta && !receipt.meta.err)
  assert.ok(receipt.slot >= drawn.firstSlot && receipt.slot <= drawn.lastSlot, 'The transfer must execute in that SIN window.')
  const view = await until('The SIN peers report both real commitment arrivals for each block', 10, async () => {
    const detail = await rpc('getSlot', { slot: receipt.slot })
    return detail.blocks.length === 3 && detail.blocks.every(block => block.commitment?.arrivals.length === 2) ? detail : false
  })
  const record = await rpc('getTransaction', { signature })
  assert.equal(view.window.hubCode, 'SIN')
  assert.equal(view.blocks.length, 3)
  assert.equal(view.agreedSet.length, 3)
  for (const block of view.blocks) {
    assert.ok(outside.has(block.identity) && block.validatorKind === 'external' && block.status === 'accepted')
    assert.ok(block.commitment && block.attestation && block.bytes > 0 && block.slices.length > 0)
    assert.equal(block.commitment.arrivals.length, 2)
    assert.ok(block.commitment.arrivals.every(arrival => arrival.onTime), 'SIN commitments must meet the same 20 ms budget.')
    const owner = operators.find(operator => operator.key.publicKey.toBase58() === block.identity)
    await until(`SIN block ${receipt.slot} is authored on ${block.identity}'s own process`, 15, async () => owner.proposed.has(receipt.slot))
  }
  const outsideBlock = view.blocks.find(block => block.identity === proposer.key.publicKey.toBase58())
  const merged = view.mergedOrder.find(item => item.id === record.header.id)
  assert.ok(merged?.selected && merged.sourceProposers.includes(outsideBlock.proposer))
  assert.equal(record.executedSlot, receipt.slot); assert.equal(record.result, 'success')
  assert.equal(await acrux.getBalance(recipient, 'finalized'), transferLamports)
  const agreement = await until('The hosted validators agree on the SIN block state', 30, async () => rpc('getExecutionAgreement', { slot: receipt.slot }))
  assert.equal(agreement.matching, true); assert.equal(agreement.mismatches, 0); assert.equal(agreement.stateHash, view.stateHash)
  await until('RPC follower and all four independent SIN nodes match this state hash', 30,
    async () => followerStates.has(receipt.slot) && operators.every(operator => operator.states.has(receipt.slot)))
  assert.equal(followerStates.get(receipt.slot), view.stateHash.slice(0, 12))
  for (const operator of operators) assert.equal(operator.states.get(receipt.slot), view.stateHash.slice(0, 12))

  const realMetrics = await until('Direct SIN co-proposers report actual signed-receipt RTT', 30, async () => {
    const snapshots = await Promise.all(operators.map(operator => getJson(`http://127.0.0.1:${operator.metricsPort}/internal/hash-exchange`)))
    const found = snapshots.flatMap((snapshot, index) => {
      assert.equal(snapshot.measurement, 'roundTrip')
      return snapshot.hubs.filter(hub => hub.hubCode === 'SIN').flatMap(hub => hub.pairs
        .filter(pair => pair.kind === 'real' && pair.count > 0 && pair.members.every(identity => outside.has(identity)))
        .map(pair => ({ reporter: operators[index], pair })))
    })
    return found.length ? found[0] : false
  })
  assert.ok(realMetrics.pair.p50 > 0 && realMetrics.pair.p99 >= realMetrics.pair.p50)
  const response = await fetchBounded(`http://127.0.0.1:${realMetrics.reporter.metricsPort}/metrics`)
  assert.ok(response.ok)
  const exposition = await response.text()
  assert.ok(exposition.split('\n').some(line => line.startsWith('acrux_hash_exchange_rtt_microseconds{')
    && line.includes('hub="SIN"') && line.includes('kind="real"')
    && realMetrics.pair.members.every(identity => line.includes(identity))), 'Prometheus must expose the real SIN member pair.')
  event('native_hub_window_verified', { hubCode: 'SIN', epoch: targetEpoch, activatedAtEpoch: frozenHub.activatedAtEpoch,
    identity: proposer.key.publicKey.toBase58(), window: drawn.number, slot: receipt.slot, signature,
    proposers: view.blocks.map(block => ({ identity: block.identity, proposer: block.proposer })), stateHash: view.stateHash,
    rpcFollowerMatches: followerStates.size, realPair: realMetrics.pair, agreement })

  const leaveEpoch = await freshEpoch(), leaving = operators.at(-1)
  const left = await send(acrux, [await registry.methods.leaveHub().accountsStrict({
    identity: leaving.key.publicKey, registry: registryAddress, hub, membership: leaving.membership,
    eventAuthority: registryEvents, program: registry.programId,
  }).instruction()], leaving.key)
  const changed = await registry.account.membership.fetch(leaving.membership), inactiveEpoch = changed.pendingEpoch.toNumber()
  assert.equal(inactiveEpoch, leaveEpoch + 1)
  const leftReceipt = await acrux.getTransaction(left, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
  assert.ok(leftReceipt?.slot < leaveEpoch * epochSlots + epochSlots / 2, 'Leave must precede the cutoff to apply at the next boundary.')
  assert.equal(hubStatus(await rpc('getValidatorSet')).active, true, 'Leaving must not deactivate the current frozen epoch early.')
  await waitSlot(leaveEpoch * epochSlots + epochSlots / 2 + 4)
  const next = await rpc('getValidatorSet', { epoch: inactiveEpoch })
  assert.ok(hubStatus(next)?.formedOutside && !hubStatus(next).active)
  const nextDraw = await rpc('getProposerEpoch', { epoch: inactiveEpoch })
  assert.ok(nextDraw.schedule.windows.every(window => window.hubCode !== 'SIN'), 'Three remaining members are below n_min=4.')
  await waitSlot(inactiveEpoch * epochSlots + 4)
  const inactive = await rpc('getValidatorSet')
  assert.ok(hubStatus(inactive)?.formedOutside && !hubStatus(inactive).active)
  const admittedMembers = inactive.members.filter(member => member.hub === 'SIN').length
  assert.equal(admittedMembers, 0, 'An inactive outside hub has no admitted voters.')
  const memberships = await Promise.all(operators.map(operator => registry.account.membership.fetch(operator.membership)))
  for (const [index, member] of memberships.entries()) {
    assert.ok(member.identity.equals(operators[index].key.publicKey) && member.hub.equals(hub) && member.hubId.eq(hubRecord.id))
    assert.ok(member.effectiveEpoch.toNumber() <= inactiveEpoch)
    if (operators[index] === leaving) {
      assert.deepEqual(Object.keys(member.pending), ['leave'])
      assert.equal(member.pendingEpoch.toNumber(), inactiveEpoch)
    } else assert.deepEqual(Object.keys(member.pending), ['none'])
  }
  const remainingRegisteredMembers = memberships.filter(member => !('leave' in member.pending)).length
  assert.equal(remainingRegisteredMembers, 3, 'The other three identities retain their effective SIN memberships.')
  event('native_hub_check_passed', { hubCode: 'SIN', registered, identities: [...outside], admittedAtEpoch: targetEpoch,
    activatedAtEpoch: frozenHub.activatedAtEpoch, window: drawn.number, slot: receipt.slot, signature,
    realPair: realMetrics.pair, left, inactiveAtEpoch: inactiveEpoch, remainingRegisteredMembers, admittedMembers,
    followerMatches: followerStates.size, peerEndpoints: peers.peers.map(peer => ({ identity: peer.identity, address: peer.address })) })
} catch (error) {
  console.error(`Local net:\n${tail}\nRPC follower:\n${followerTail}`)
  for (const operator of operators) console.error(`SIN ${operator.key.publicKey.toBase58()}:\n${operator.tail}`)
  throw error
} finally {
  const failures = []
  for (const owned of [follower, ...operators.map(operator => operator.child).reverse(), child]) {
    if (!owned?.pid || owned.exitCode !== null || owned.signalCode !== null) continue
    try {
      const exited = new Promise(resolve => owned.once('exit', resolve))
      owned.kill('SIGTERM'); await Promise.race([exited, pause(5000)])
      if (owned.exitCode === null && owned.signalCode === null) { owned.kill('SIGKILL'); await Promise.race([exited, pause(5000)]) }
      assert.ok(owned.exitCode !== null || owned.signalCode !== null, 'A check-owned process did not stop.')
    } catch (error) { failures.push(error) }
  }
  if (runDir && !failures.length) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must stay inside node/target.')
    rmSync(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
  if (failures.length) throw new AggregateError(failures, 'Check-owned process cleanup failed.')
}
