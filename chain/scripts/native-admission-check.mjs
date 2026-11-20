// Build 5 step 2 on an operator's fresh local Solana validator at 4990, loaded with
// the rebuilt native bridge. This check never deploys, resets or stops that validator.
// It starts its own Acrux net (TCP 5041..5043, QUIC 5062..5073 and observer QUIC 5074), uses the checked-in
// Stake placement, and removes its two children and unique node/target store on exit.
// ACRUX_IDENTITY_SEED must be a private LOCAL-check seed. All rule values are explicit:
// node scripts/native-admission-check.mjs --native-from-slot 8192 --stake-from-slot 8448 --admission-from-slot 8704 --epoch-slots 256 --native-scale 1000 --genesis-stake-lamports 1000000000
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
import { bridgeProgram, pdas, seed, relay, decodeEntry, chainTag, programData } from './bridge-relay.mjs'

const arg = name => { const i = process.argv.indexOf(`--${name}`); return i < 0 ? undefined : process.argv[i + 1] }
// --silent-at-boundary: the admitted member's node stops just before its admission boundary.
const silentAtBoundary = process.argv.includes('--silent-at-boundary')
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
assert.ok(epochSlots % 16 === 0, 'The local bridge epoch must contain whole sixteen-slot proposer windows.')
assert.ok(nativeFromSlot % epochSlots === 0 && stakeFromSlot % epochSlots === 0 && stakeFromSlot > nativeFromSlot, 'Use separate native and stake activation boundaries in the local bridge epochs.')
assert.ok(admissionFromSlot % epochSlots === 0 && admissionFromSlot > stakeFromSlot, 'Admission must start at a later local epoch boundary than Stake activation.')
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
let runDir, child, observer, tail = '', observerTail = '', interrupted = false, childError
const observerEpochs = new Set(), memberEpochs = new Set(), signedEpochs = new Set()
let observerMatches = 0, observerLine = ''
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function ownNet() {
  if (interrupted) throw new Error('Native stake check interrupted.')
  if (childError) throw childError
  if (child && (child.exitCode !== null || child.signalCode !== null)) throw new Error(`The local net exited: ${tail.slice(-6000)}`)
  if (observer && (observer.exitCode !== null || observer.signalCode !== null)) throw new Error(`The outside observer exited: ${observerTail.slice(-6000)}`)
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
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen({ port, host: '127.0.0.1', exclusive: true }, resolve) })
    await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()))
  }
  for (let port = 5062; port <= 5074; port++) {
    const socket = dgram.createSocket('udp4')
    try { await new Promise((resolve, reject) => { socket.once('error', reject); socket.bind({ port, address: '127.0.0.1', exclusive: true }, resolve) }) }
    finally { try { socket.close() } catch (error) { if (error.code !== 'ERR_SOCKET_DGRAM_NOT_RUNNING') throw error } }
  }
}

try {
  await freePorts()
  assert.ok(await solana.getAccountInfo(program, 'finalized'), 'Load the rebuilt native bridge on the local validator first.')
  assert.equal(await solana.getAccountInfo(addresses.bridge, 'finalized'), null, 'Use a fresh local validator; this check never resets it.')
  // Initialize for the admission epoch now; deposits do not require a seeded light client.
  // This avoids relaying every pre-native epoch just to reach the admission under test.
  const chainId = 'acrux-native-admission-local-check'
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
  runDir = mkdtempSync(join(target, 'native-admission-check-'))
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
  event('native_admission_deposited', { deposited, mint: mint.toBase58(), bridgedBaseUnits: depositAmount.toString(), nativeFromSlot, stakeFromSlot, epochSlots })
  await waitSlot(nativeFromSlot + 4)
  const supply = await rpc('getNativeSupply')
  assert.equal(BigInt(supply.creditedLamports), depositAmount * scale)
  assert.equal(BigInt(await acrux.getBalance(holder.publicKey, 'finalized')), depositAmount * scale)
  assert.equal((await token(oldSource)).amount, 0n)
  await waitSlot(stakeFromSlot + 4)
  assert.ok((await acrux.getAccountInfo(StakeProgram.programId, 'finalized'))?.executable, 'The named Stake placement must be active.')

  const holderFile = join(runDir, 'outside-keypair.json')
  writeFileSync(holderFile, JSON.stringify([...holder.secretKey]), { mode: 0o600 })
  observer = spawn(binary, ['node', 'propose', '--hub', 'FRA', '--identity', holderFile, '--net', gateway, '--genesis', genesis],
    { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  observer.once('error', error => { childError = error })
  observer.stderr.on('data', data => { observerTail = (observerTail + data).slice(-64_000) })
  observer.stdout.on('data', data => {
    observerTail = (observerTail + data).slice(-64_000); observerLine += data
    let newline
    while ((newline = observerLine.indexOf('\n')) >= 0) {
      const line = observerLine.slice(0, newline); observerLine = observerLine.slice(newline + 1)
      const matching = /^slot (\d+) re-executed\b/.exec(line)
      if (matching) {
        if (!/\bmatches\b/.test(line)) childError = new Error(`Outside replay mismatch: ${line}`)
        observerMatches++
      }
      const membership = /^epoch (\d+) member /.exec(line)
      const observing = /^epoch (\d+) observer /.exec(line)
      const vote = /^vote (?:Notar|Finalize) cblock \d+ epoch (\d+)/.exec(line)
      if (membership) memberEpochs.add(Number(membership[1]))
      if (observing) observerEpochs.add(Number(observing[1]))
      if (vote) signedEpochs.add(Number(vote[1]))
    }
  })
  await until('The outside identity independently re-executes before admission', 60, async () => observerMatches >= 8)
  await waitSlot(admissionFromSlot + 4)
  const activationEpoch = admissionFromSlot / epochSlots
  const initial = await rpc('getValidatorSet', { epoch: activationEpoch })
  assert.equal(initial.members.length, 12)
  assert.ok(!initial.members.some(member => member.identity === holder.publicKey.toBase58()))
  const firstAdmissionProof = await rpc('getCblockProof', { latest: true })
  assert.equal(Math.floor(firstAdmissionProof.consensusSlot * 4 / epochSlots), activationEpoch, 'Use a longer local epoch if initial setup crossed the first admission epoch.')
  await seed(onSolana, firstAdmissionProof, epochSlots)
  const hubIdl = JSON.parse(readFileSync(join(chainDir, 'target/idl/hub_registry.json'), 'utf8'))
  const registry = new anchor.Program(hubIdl, new anchor.AnchorProvider(acrux, new anchor.Wallet(holder), { commitment: 'confirmed' }))
  const rpda = (...seeds) => PublicKey.findProgramAddressSync(seeds, registry.programId)[0]
  const membership = rpda(Buffer.from('member'), holder.publicKey.toBuffer())
  const hub = rpda(Buffer.from('hub'), Buffer.from('FRA'))
  const registryAddress = rpda(Buffer.from('registry')), registryEvents = rpda(Buffer.from('__event_authority'))
  const joined = await send(acrux, [await registry.methods.joinHub().accountsStrict({
    payer: holder.publicKey, identity: holder.publicKey, registry: registryAddress, hub, membership,
    systemProgram: SystemProgram.programId, eventAuthority: registryEvents, program: registry.programId,
  }).instruction()], holder)
  const joinedState = await registry.account.membership.fetch(membership)
  const effectiveEpoch = joinedState.effectiveEpoch.toNumber()
  assert.ok(effectiveEpoch > activationEpoch)
  const voteAddress = PublicKey.findProgramAddressSync([Buffer.from('acrux_genesis_vote'), holder.publicKey.toBuffer()], VoteProgram.programId)[0]
  assert.equal(await acrux.getAccountInfo(voteAddress, 'finalized'), null, 'Pending membership must not have its Vote account installed early.')
  const relayNext = async epoch => {
    const proof = await until(`The epoch ${epoch} finalized cutoff carries epoch ${epoch + 1}`, Math.ceil(epochSlots / 20) + 90, async () => {
      try { return await rpc('getCblockProof', { epoch }) }
      catch (error) { if (error.rpcCode === 'NOT_FOUND') return false; throw error }
    })
    const next = proof.outbox.map(entry => decodeEntry(Buffer.from(entry, 'base64'))).find(entry => entry && !entry.withdrawal && !entry.feeBurn && entry.epoch.toNumber() === epoch + 1)
    assert.ok(next, 'The retained epoch representative must contain the frozen next set.')
    const relayed = await relay(onSolana, proof)
    const stored = await onSolana.account.validatorSet.fetch(addresses.set(epoch + 1), 'confirmed')
    assert.deepEqual(stored.validators.map(member => [member.key.toBase58(), member.stake.toString()]), next.validators.map(member => [member.key.toBase58(), member.stake.toString()]))
    event('native_admission_set_handed_on', { sourceEpoch: epoch, targetEpoch: epoch + 1, cblock: proof.consensusSlot, relayed })
    return { proof, next }
  }
  for (let epoch = activationEpoch; epoch < effectiveEpoch; epoch++) await relayNext(epoch)
  await waitSlot(effectiveEpoch * epochSlots + 4)
  await until('Effective membership installs its real Vote account', 30, async () => {
    const account = await acrux.getAccountInfo(voteAddress, 'finalized')
    return account && account.owner.equals(VoteProgram.programId) && !account.executable
  })
  assert.ok(!(await rpc('getValidatorSet')).members.some(member => member.identity === holder.publicKey.toBase58()), 'Membership alone must not admit an unstaked identity.')
  const rent = await acrux.getMinimumBalanceForRentExemption(StakeProgram.space)
  const minimum = await acrux.getStakeMinimumDelegation({ commitment: 'finalized' })
  assert.ok(stakeLamports >= BigInt(minimum.value), 'Twice --genesis-stake-lamports must meet the actual Stake program minimum.')
  const create = StakeProgram.createAccount({ fromPubkey: holder.publicKey, stakePubkey: stakeKeypair.publicKey,
    authorized: new Authorized(holder.publicKey, holder.publicKey), lamports: asNumber(stakeLamports + BigInt(rent), 'stake account funding') })
  const delegate = StakeProgram.delegate({ stakePubkey: stakeKeypair.publicKey, authorizedPubkey: holder.publicKey, votePubkey: voteAddress })
  const delegated = await send(acrux, [...create.instructions, ...delegate.instructions], holder, [stakeKeypair])
  const stake = await stakeAccount()
  assert.ok(stake.voter.equals(voteAddress)); assert.equal(stake.stake, stakeLamports)
  const delegatedEpoch = Number(stake.activationEpoch), targetEpoch = delegatedEpoch + 1
  const receipt = await acrux.getTransaction(delegated, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
  assert.ok(receipt && receipt.slot < delegatedEpoch * epochSlots + epochSlots / 2, 'Delegation must finalize before the midpoint; use a longer local epoch on a slower machine.')
  for (let epoch = effectiveEpoch; epoch < targetEpoch; epoch++) await relayNext(epoch)
  const frozen = await rpc('getValidatorSet', { epoch: targetEpoch })
  const external = frozen.members.find(member => member.identity === holder.publicKey.toBase58())
  assert.ok(external && external.validatorKind === 'external')
  assert.equal(BigInt(external.stake), stakeLamports)
  assert.equal(external.admittedAtEpoch, targetEpoch)
  assert.ok(frozen.cutoffSlot < targetEpoch * epochSlots)
  assert.ok(!(await rpc('getValidatorSet')).members.some(member => member.identity === holder.publicKey.toBase58()), 'The frozen next set must not activate early.')
  const onSolanaNext = await onSolana.account.validatorSet.fetch(addresses.set(targetEpoch), 'confirmed')
  assert.equal(onSolanaNext.validators.find(member => member.key.equals(holder.publicKey)).stake.toString(), stakeLamports.toString())
  assert.ok((await rpc('getFinalizedCblock')).consensusSlot * 4 < targetEpoch * epochSlots, 'The next set must be handed to Solana before its boundary.')
  if (silentAtBoundary) {
    // The case live can meet first: a member admitted at a boundary that is silent there. Its node
    // stops a few slots before the boundary, and the hosted net must finalize through it and past the next.
    await waitSlot(targetEpoch * epochSlots - 8)
    const stopped = observer
    observer = null
    stopped.kill()
    const stoppedAt = (await rpc('getFinalizedCblock')).consensusSlot * 4
    let last = stoppedAt, lastAt = Date.now(), longestStallMs = 0
    await until(`Finality through the boundary at ${targetEpoch * epochSlots} and past ${(targetEpoch + 1) * epochSlots}`, Math.ceil(3 * epochSlots / 15) + 120, async () => {
      const head = (await rpc('getFinalizedCblock')).consensusSlot * 4
      if (head > last) { longestStallMs = Math.max(longestStallMs, Date.now() - lastAt); last = head; lastAt = Date.now() }
      assert.ok(Date.now() - lastAt < 30_000, `Finality stood still for 30 s at slot ${last} with the admitted member silent.`)
      return head >= (targetEpoch + 1) * epochSlots + 4 && head
    })
    const set = await rpc('getValidatorSet', { epoch: targetEpoch })
    assert.ok(set.members.some(member => member.identity === holder.publicKey.toBase58()), 'The silent member is in the set it stays silent in.')
    event('native_admission_silent_member_passed', { identity: holder.publicKey.toBase58(), admittedAtEpoch: targetEpoch, stoppedAtSlot: stoppedAt,
      boundary: targetEpoch * epochSlots, nextBoundary: (targetEpoch + 1) * epochSlots, finalizedThrough: last, longestStallMs, stake: stakeLamports.toString() })
    process.exitCode = 0
    throw Object.assign(new Error('silent member check done'), { done: true })
  }
  await waitSlot(targetEpoch * epochSlots + 4)
  await until('Outside identity enters voting mode and signs', 30, async () => memberEpochs.has(targetEpoch) && signedEpochs.has(targetEpoch))
  const voted = await until('A finalized proof includes the outside Finalize vote', 30, async () => {
    const proof = await rpc('getCblockProof', { latest: true })
    if (Math.floor(proof.consensusSlot * 4 / epochSlots) !== targetEpoch) return false
    const index = proof.validators.indexOf(holder.publicKey.toBase58())
    const vote = proof.votes.find(vote => vote.validator === index && vote.kind === 'finalize')
    return vote ? { proof, index, vote } : false
  })
  // Make the external signature the first Finalize entry so Solana actually counts its bit,
  // even when enough genesis votes alone could otherwise certify the same cblock.
  const report = await relay(onSolana, { ...voted.proof, votes: [voted.vote, ...voted.proof.votes.filter(vote => vote !== voted.vote)] })
  const certified = await onSolana.account.cblock.fetch(addresses.cblock(Buffer.from(voted.proof.hash, 'hex')), 'confirmed')
  assert.ok(certified.certified && (certified.finalize >>> voted.index & 1), 'Solana must count the outside validator signature against the admitted set.')
  const leaving = await send(acrux, [await registry.methods.leaveHub().accountsStrict({
    identity: holder.publicKey, registry: registryAddress, hub, membership, eventAuthority: registryEvents, program: registry.programId,
  }).instruction()], holder)
  const leavingState = await registry.account.membership.fetch(membership)
  const leaveEpoch = leavingState.pendingEpoch.toNumber()
  const leaveReceipt = await acrux.getTransaction(leaving, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
  assert.ok(leaveReceipt && leaveReceipt.slot < (leaveEpoch - 1) * epochSlots + epochSlots / 2, 'Leave must finalize before the midpoint; use a longer local epoch on a slower machine.')
  const beforeLeave = await rpc('getValidatorSet')
  assert.ok(beforeLeave.members.some(member => member.identity === holder.publicKey.toBase58()), 'Leave must not remove current-epoch voting weight early.')
  await relayNext(leaveEpoch - 1)
  assert.ok(!(await rpc('getValidatorSet', { epoch: leaveEpoch })).members.some(member => member.identity === holder.publicKey.toBase58()))
  await waitSlot(leaveEpoch * epochSlots + 20)
  await until('The leaving identity returns to observer mode', 30, async () => observerEpochs.has(leaveEpoch))
  assert.ok(!signedEpochs.has(leaveEpoch), 'A departed identity must stop voting at its effective boundary.')
  const after = await rpc('getCblockProof', { latest: true })
  assert.ok(!after.validators.includes(holder.publicKey.toBase58()))
  const afterMatches = observerMatches
  await until('The outside process keeps re-executing after leave', 30, async () => observerMatches >= afterMatches + 8)
  event('native_admission_check_passed', { joined, delegated, leaving, identity: holder.publicKey.toBase58(), vote: voteAddress.toBase58(),
    nativeFromSlot, stakeFromSlot, admissionFromSlot, epochSlots, membershipEffectiveEpoch: effectiveEpoch, admittedAtEpoch: targetEpoch,
    leftAtEpoch: leaveEpoch, nativeDelegatedLamports: stakeLamports.toString(), outsideFinalizeCblock: voted.proof.consensusSlot,
    solanaCertificate: report, observerMatches })
} catch (error) {
  if (!error.done) {
    console.error(`The local net's last output:\n${tail}\nOutside process:\n${observerTail}`)
    throw error
  }
} finally {
  const failures = []
  for (const owned of [observer, child]) {
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
