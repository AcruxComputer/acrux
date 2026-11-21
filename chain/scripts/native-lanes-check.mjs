// R23 / Build 6 step 6: lanes activation across hosted validators, followers and one outside member.
// Operator-run only, against a fresh local Solana validator at 4990 already loaded with the unchanged native bridge.
// This script never deploys, starts, resets or stops that validator. It owns only its Acrux processes,
// TCP 5041..5043/5076 and QUIC 5062..5074, and cleans its unique node/target directory.
// Example from backend/chain (private LOCAL-check ACRUX_IDENTITY_SEED required):
// node scripts/native-lanes-check.mjs --native-from-slot 2048 --stake-from-slot 2560 --admission-from-slot 3072 --lanes-from-slot 6144 --epoch-slots 512 --native-scale 1000 --genesis-stake-lamports 1000000000
// At 20 slots/s the example reaches lanes after about 5m07s; allow about 7 to 10 minutes including local Solana setup/relay.
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { createHash, createPublicKey, verify } from 'node:crypto'
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
import { bridgeProgram, pdas, seed, relay, decodeEntry, chainTag, programData, voteMessage } from './bridge-relay.mjs'

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
const lanesFromSlot = asNumber(required('lanes-from-slot'), 'lanes-from-slot')
const scale = required('native-scale'), genesisStakeLamports = required('genesis-stake-lamports')
asNumber(scale, 'native-scale')
assert.ok(epochSlots % 32 === 0, 'The local epoch midpoint must be a proposer-window boundary.')
assert.ok(nativeFromSlot % epochSlots === 0 && stakeFromSlot % epochSlots === 0 && stakeFromSlot > nativeFromSlot, 'Use separate native and stake activation boundaries in the local bridge epochs.')
assert.ok(admissionFromSlot % epochSlots === 0 && admissionFromSlot > stakeFromSlot, 'Admission must start at a later local epoch boundary than Stake activation.')
assert.ok(lanesFromSlot % epochSlots === 0 && lanesFromSlot >= admissionFromSlot + 4 * epochSlots, 'Set lanes at least four local epochs after admission so the outside member can join, delegate and produce before activation.')
// Optional: the native mint and the epoch fee burn in the same rule, as they go on the net.
const optional = name => arg(name) === undefined ? undefined : asNumber(required(name), name)
const nativeMintFromSlot = optional('native-mint-from-slot'), feeBurnFromSlot = optional('fee-burn-from-slot')
assert.ok(feeBurnFromSlot === undefined || feeBurnFromSlot % epochSlots === 0 && feeBurnFromSlot > lanesFromSlot, 'Set the fee burn at a local epoch boundary after lanes.')
// Optional: the genesis weight steps down by admitted outside stake from this boundary.
const genesisStepFromSlot = optional('genesis-step-from-slot')
assert.ok(genesisStepFromSlot === undefined || genesisStepFromSlot % epochSlots === 0 && genesisStepFromSlot >= Math.max(lanesFromSlot, feeBurnFromSlot ?? 0) + 4 * epochSlots,
  'Set the genesis step at a local epoch boundary at least four epochs after lanes and the fee burn, so a second outside member can join and stake before it.')
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
const onAcrux = bridgeProgram(acrux, holder)
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const u64 = value => { const data = Buffer.alloc(8); data.writeBigUInt64LE(BigInt(value.toString())); return data }
const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
const source = getAssociatedTokenAddressSync(mint, authority.publicKey)
const oldSource = getAssociatedTokenAddressSync(bridgedMint, holder.publicKey)
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const event = (name, fields) => console.log(JSON.stringify({ event: name, ...fields }))
let runDir, child, observer, follower, tail = '', observerTail = '', followerTail = '', followerLine = '', interrupted = false, childError
const observerEpochs = new Set(), memberEpochs = new Set(), signedEpochs = new Set()
const observerStates = new Map(), laneVotes = new Set()
let observerMatches = 0, observerLine = ''
const followerStates = new Map()
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function ownNet() {
  if (interrupted) throw new Error('Native lanes check interrupted.')
  if (childError) throw childError
  if (child && (child.exitCode !== null || child.signalCode !== null)) throw new Error(`The local net exited: ${tail.slice(-6000)}`)
  if (observer && (observer.exitCode !== null || observer.signalCode !== null)) throw new Error(`The outside observer exited: ${observerTail.slice(-6000)}`)
  if (follower && (follower.exitCode !== null || follower.signalCode !== null)) throw new Error(`The RPC follower exited: ${followerTail.slice(-6000)}`)
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
const sha = (...parts) => createHash('sha256').update(Buffer.concat(parts)).digest()
const vector = (items, encode) => Buffer.concat([u64(items.length), ...items.map(encode)])
const option = (value, encode) => value === null || value === undefined ? Buffer.from([0]) : Buffer.concat([Buffer.from([1]), encode(value)])
const text = value => { const bytes = Buffer.from(value); return Buffer.concat([u64(bytes.length), bytes]) }
// The exact historical bincode tuple: (cblock, Option<BlockRef>, Vec<(slot, Option<Vec<usize>>, Vec<Option<Hash>>)>, stateHexString).
function legacyPreimage(views) {
  const last = views.at(-1), cblock = last.cblock
  return Buffer.concat([u64(cblock.consensusSlot), option(cblock.parent, parent => Buffer.concat([u64(parent.consensusSlot), Buffer.from(parent.hash, 'hex')])),
    vector(views, view => {
      const first = [...view.attestations].sort((a, b) => a.proposer - b.proposer)[0]
      assert.ok(first && view.agreedSet && view.stateHash, 'The legacy cblock must have executed slot records.')
      return Buffer.concat([u64(view.slot), option(view.agreedSet, accepted => vector(accepted, u64)),
        vector(first.pi, hash => option(hash, value => Buffer.from(value, 'hex')))])
    }), text(last.stateHash)])
}
async function stopOwned(owned) {
  if (!owned?.pid || owned.exitCode !== null || owned.signalCode !== null) return
  const exited = new Promise(resolve => owned.once('exit', resolve))
  owned.kill('SIGTERM'); await Promise.race([exited, pause(5000)])
  if (owned.exitCode === null && owned.signalCode === null) { owned.kill('SIGKILL'); await Promise.race([exited, pause(5000)]) }
  assert.ok(owned.exitCode !== null || owned.signalCode !== null, 'A check-owned process did not stop.')
}
async function freePorts() {
  for (const port of [5041, 5042, 5043, 5076]) {
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
  assert.ok(await solana.getAccountInfo(program, 'finalized'), 'Load the existing native bridge on the local validator first.')
  const programBefore = await solana.getAccountInfo(programData(program), 'finalized')
  assert.ok(programBefore, 'The existing local bridge program data must be readable.')
  assert.equal(await solana.getAccountInfo(addresses.bridge, 'finalized'), null, 'Use a fresh local validator; this check never resets it.')
  // Initialize for the admission epoch now; deposits do not require a seeded light client.
  // This avoids relaying every pre-native epoch just to reach the admission under test.
  const chainId = 'acrux-native-lanes-local-check'
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
  if (feeBurnFromSlot !== undefined) await send(solana, [await onSolana.methods.initFeeBurnConfig().accountsStrict({ authority: authority.publicKey, bridge: addresses.bridge, mint, config: addresses.feeBurnConfig, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).instruction()], authority)
  runDir = mkdtempSync(join(target, 'native-lanes-check-'))
  const rule = { program: program.toBase58(), fromSlot: 0, epochSlots, chainId, nativeFromSlot, nativeMint: mint.toBase58(), nativeScale: Number(scale), stakeFromSlot, genesisStakeLamports: genesisStakeLamports.toString(), admissionFromSlot, lanesFromSlot, nativeMintFromSlot, feeBurnFromSlot, genesisStepFromSlot }
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
  // Its own RPC port takes the transfer below, so the outside block is the one that carries it.
  observer = spawn(binary, ['node', 'propose', '--hub', 'FRA', '--identity', holderFile, '--net', gateway, '--genesis', genesis, '--rpc-port', '5076'],
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
        const state = /state (\S+) matches/.exec(line)
        if (state) observerStates.set(Number(matching[1]), state[1])
      }
      const laneVote = /^lane vote (?:Notar|Finalize|Skip) proposer (\d+) cblock (\d+) epoch (\d+)/.exec(line)
      if (laneVote) laneVotes.add(`${laneVote[1]}/${laneVote[2]}`)
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
    return { proof, next, relayed }
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
  await waitSlot(targetEpoch * epochSlots + 4)
  await until('Outside identity enters voting mode and signs', 30, async () => memberEpochs.has(targetEpoch) && signedEpochs.has(targetEpoch))

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
      if (matched) followerStates.set(Number(matched[1]), matched[2])
      else if (/^slot \d+ re-executed\b/.test(line)) childError = new Error(`RPC follower mismatch: ${line}`)
    }
  })
  await until('RPC follower is independently matching finalized inputs', 60, async () => followerStates.size >= 8)
  async function checkedSlot(slot, label) {
    const view = await rpc('getSlot', { slot })
    const agreement = await until(`${label}: all twelve hosted state hashes`, 30, () => rpc('getExecutionAgreement', { slot }))
    assert.equal(agreement.validators, 12); assert.equal(agreement.matching, true); assert.equal(agreement.mismatches, 0)
    assert.equal(agreement.stateHash, view.stateHash)
    await until(`${label}: RPC and outside followers match`, 30, async () => followerStates.has(slot) && observerStates.has(slot))
    assert.equal(followerStates.get(slot), view.stateHash.slice(0, 12)); assert.equal(observerStates.get(slot), view.stateHash.slice(0, 12))
    return { view, agreement }
  }
  async function outsidePayment(after) {
    const drawn = await until(`An admitted outside block ${after ? 'after' : 'before'} lanes activation`, 120, async () => {
      const window = await rpc('getCurrentWindow'), proposer = window.proposers.find(proposer => proposer.identity === holder.publicKey.toBase58())
      const latest = await acrux.getSlot('processed')
      if (!proposer || latest < window.firstSlot || window.lastSlot - latest < 8) return false
      if (!after && window.lastSlot >= lanesFromSlot) throw new Error('The pre-activation proposal missed its configured window; choose a later explicit lanes slot.')
      if (after && window.firstSlot < lanesFromSlot) return false
      assert.equal(proposer.validatorKind, 'external')
      return { window, proposer }
    })
    const recipient = Keypair.generate().publicKey, rent = await acrux.getMinimumBalanceForRentExemption(0)
    const latest = await acrux.getLatestBlockhash('confirmed')
    const transaction = new Transaction({ feePayer: holder.publicKey, ...latest }).add(SystemProgram.transfer({ fromPubkey: holder.publicKey, toPubkey: recipient, lamports: rent }))
    transaction.sign(holder)
    // Preserve the existing operator check's direct --rpc-port 5076 submission.
    const signature = await connection('http://127.0.0.1:5076').sendRawTransaction(transaction.serialize(), { skipPreflight: true, maxRetries: 0 })
    await until('The outside-member transfer finalizes', 60, async () => {
      const status = (await acrux.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
      if (status?.err) throw new Error(`Outside transfer failed: ${JSON.stringify(status.err)}`)
      return status?.confirmationStatus === 'finalized'
    })
    const receipt = await acrux.getTransaction(signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
    assert.ok(receipt?.meta && !receipt.meta.err)
    assert.ok(receipt.slot >= drawn.window.firstSlot && receipt.slot <= drawn.window.lastSlot)
    assert.equal(receipt.slot >= lanesFromSlot, after)
    const { view, agreement } = await checkedSlot(receipt.slot, 'Outside proposal')
    const block = view.blocks.find(block => block.identity === holder.publicKey.toBase58())
    assert.ok(block && block.status === 'accepted' && block.validatorKind === 'external' && block.commitment && block.attestation)
    const record = await rpc('getTransaction', { signature })
    const merged = view.mergedOrder.find(tx => tx.id === record.header.id)
    assert.ok(merged?.selected && merged.sourceProposers.includes(block.proposer) && view.agreedSet.includes(block.proposer))
    assert.equal(await acrux.getBalance(recipient, 'finalized'), rent)
    if (after) {
      const lane = view.cblock.lanes?.find(lane => lane.proposer === block.proposer)
      assert.ok(lane && lane.status === 'finalized' && Object.values(lane.votes).some(count => count > 0))
      await until('The outside process casts its own lane votes', 30, async () => laneVotes.has(`${block.proposer}/${view.cblock.consensusSlot}`))
      const epoch = Math.floor(receipt.slot / epochSlots), cblock = view.cblock.consensusSlot
      const voting = await rpc('getValidatorSet', { epoch })
      const index = voting.members.findIndex(member => member.identity === holder.publicKey.toBase58())
      assert.ok(index >= 0, 'The frozen epoch set must give the outside signer a compact voting index.')
      const signed = await until('The outside vote is counted in its finalized lane certificate', 30, async () => {
        const page = await rpc('getFollowerPage', { from: cblock * 4, count: 4 })
        const material = page.slots.find(row => row.slot === cblock * 4 + 3)?.blocks?.laneCblock
        return material?.lanes.find(lane => lane.lane === block.proposer)?.proofs.find(proof =>
          proof.vote.validator === index && proof.vote.slot === cblock && ['notar', 'finalize'].includes(proof.vote.kind))
      })
      assert.equal(signed.signature.length, 64)
      const key = createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), holder.publicKey.toBuffer()]), format: 'der', type: 'spki' })
      const message = Buffer.concat([chainTag(chainId), Buffer.from('acrux-lane-v1'), Buffer.from([block.proposer]),
        voteMessage(Buffer.alloc(0), index, cblock, Buffer.from(signed.vote.hash), signed.vote.kind)])
      assert.ok(verify(null, message, key, Buffer.from(signed.signature)), 'The counted lane signature must verify in its chain and proposer domain.')
    } else {
      assert.equal(view.cblock.lanes, undefined, 'Pre-activation public cblocks keep their historical shape.')
      const views = []
      for (let slot = view.cblock.consensusSlot * 4; slot < view.cblock.consensusSlot * 4 + 4; slot++) views.push(await rpc('getSlot', { slot }))
      const proof = await rpc('getCblockProof', { consensusSlot: view.cblock.consensusSlot })
      const expected = legacyPreimage(views)
      assert.deepEqual(Buffer.from(proof.preimage, 'base64'), expected, 'The cblock before activation must use the exact historical bincode preimage.')
      assert.equal(sha(expected, Buffer.from(proof.outboxRoot, 'hex')).toString('hex'), proof.hash)
    }
    return { signature, slot: receipt.slot, cblock: view.cblock.consensusSlot, proposer: block.proposer, agreement }
  }
  assert.ok((await rpc('getFinalizedCblock')).consensusSlot * 4 + 3 < lanesFromSlot - 16, 'Choose a later explicit lanes boundary if admission consumed the pre-activation interval.')
  const before = await outsidePayment(false)
  await waitSlot(lanesFromSlot + 4)
  const boundaryBefore = await checkedSlot(lanesFromSlot - 1, 'Last historical slot')
  const boundaryAfter = await checkedSlot(lanesFromSlot + 3, 'First lanes cblock')
  assert.equal(boundaryBefore.view.cblock.lanes, undefined)
  assert.equal(boundaryAfter.view.cblock.lanes.length, 3)
  const after = await outsidePayment(true)

  // A lanes cblock still has an execution certificate accepted by the unchanged Solana program.
  const recipient = holder.publicKey, destination = getAssociatedTokenAddressSync(mint, recipient)
  const native = pda(Buffer.from('native')), burn = pda(Buffer.from('native_burn'))
  const withdrawalMinimum = pda(Buffer.from('minimum'), mint.toBuffer()), outbound = pda(Buffer.from('outbound'))
  const withdrawalSequence = (await onAcrux.account.outbound.fetchNullable(outbound))?.withdrawals ?? new anchor.BN(0)
  const record = pda(Buffer.from('withdrawal'), u64(withdrawalSequence)), withdrawAmount = depositAmount / 16n
  assert.ok(withdrawAmount > 0n)
  const withdrawal = await send(acrux, [await onAcrux.methods.withdrawNative(new anchor.BN(withdrawAmount.toString()), recipient).accountsStrict({
    owner: holder.publicKey, native, minimum: withdrawalMinimum, burn, outbound, record, systemProgram: SystemProgram.programId,
  }).instruction()], holder)
  const made = await onAcrux.account.withdrawal.fetch(record, 'finalized'), withdrawalSlot = made.slot.toNumber()
  assert.ok(withdrawalSlot >= lanesFromSlot)
  const cblock = Math.floor(withdrawalSlot / 4), proof = await until('The retained lanes withdrawal proof', 60, () => rpc('getCblockProof', { consensusSlot: cblock }))
  const withdrawalView = await rpc('getSlot', { slot: withdrawalSlot })
  assert.equal(withdrawalView.cblock.lanes.length, 3)
  assert.equal(Number(Buffer.from(proof.preimage, 'base64').readBigUInt64LE(0)), cblock, 'The unchanged Solana program still reads the cblock slot in the first eight bytes.')
  assert.equal(sha(Buffer.from(proof.preimage, 'base64'), Buffer.from(proof.outboxRoot, 'hex')).toString('hex'), proof.hash)
  const releaseReports = []
  for (let epoch = targetEpoch; epoch < Math.floor(withdrawalSlot / epochSlots); epoch++) releaseReports.push((await relayNext(epoch)).relayed)
  releaseReports.push(await relay(onSolana, proof))
  assert.ok(releaseReports.some(report => report.released.some(entry => entry.sequence === withdrawalSequence.toNumber())), 'A certified lanes cblock must release this withdrawal exactly once.')
  await until('The Solana payout is finalized in the recipient token account', 90, async () => {
    const paid = await solana.getAccountInfo(destination, 'finalized')
    return paid && unpackAccount(destination, paid, TOKEN_PROGRAM_ID).amount === withdrawAmount
  })
  assert.deepEqual((await solana.getAccountInfo(programData(program), 'finalized')).data, programBefore.data, 'No bridge program upgrade is involved.')

  let nativeMint, feeBurned
  if (nativeMintFromSlot !== undefined) {
    await waitSlot(nativeMintFromSlot + 4)
    const placed = await acrux.getAccountInfo(new PublicKey('So11111111111111111111111111111111111111112'), 'finalized')
    assert.ok(placed && placed.owner.equals(TOKEN_PROGRAM_ID) && placed.data.length === MINT_SIZE && placed.data[44] === 9 && placed.data[45] === 1, 'The native mint must be placed from its slot.')
    nativeMint = { lamports: placed.lamports }
  }
  if (feeBurnFromSlot !== undefined) {
    // Base fees of the first fee-burn epoch, on lanes cblocks, burned through the program on Solana.
    const burnEpoch = feeBurnFromSlot / epochSlots
    await waitSlot(feeBurnFromSlot + 4)
    const destination = Keypair.generate().publicKey, burnTransfers = []
    for (let i = 0; i < 3; i++) {
      const signature = await send(acrux, [SystemProgram.transfer({ fromPubkey: holder.publicKey, toPubkey: destination, lamports: 2_000_000 })], holder)
      const receipt = await acrux.getTransaction(signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
      assert.equal(Math.floor(receipt.slot / epochSlots), burnEpoch, 'Choose a later fee-burn epoch if the transfers missed it.')
      burnTransfers.push(signature)
    }
    for (let epoch = Math.floor(withdrawalSlot / epochSlots); epoch < burnEpoch; epoch++) await relayNext(epoch)
    await waitSlot((burnEpoch + 1) * epochSlots + 4)
    const burnProof = await until('The fee-burn epoch-ending proof', 60, () => rpc('getCblockProof', { feeBurnEpoch: burnEpoch }))
    const ending = await rpc('getSlot', { slot: burnProof.consensusSlot * 4 })
    assert.equal(ending.cblock.lanes.length, 3, 'The epoch-ending cblock is a lanes cblock.')
    const leaves = burnProof.outbox.map(bytes => decodeEntry(Buffer.from(bytes, 'base64'))).filter(entry => entry?.feeBurn)
    assert.equal(leaves.length, 1)
    assert.equal(leaves[0].epoch.toNumber(), burnEpoch)
    const amount = BigInt(leaves[0].amount.toString())
    assert.ok(amount > 0n, 'The three transfers must burn at least one Solana base unit.')
    const supplyBefore = BigInt((await solana.getTokenSupply(mint, 'finalized')).value.amount)
    const relayed = await relay(onSolana, burnProof)
    assert.deepEqual(relayed.burned, [{ epoch: burnEpoch, amount: amount.toString() }])
    assert.ok(await onSolana.account.validatorSet.fetchNullable(addresses.set(burnEpoch + 1), 'confirmed'), 'The epoch-ending cblock still hands the next set on.')
    // The relay confirms its burn; the finalized supply follows a few slots later.
    await until('The burn is finalized in the mint supply', 60, async () => BigInt((await solana.getTokenSupply(mint, 'finalized')).value.amount) === supplyBefore - amount)
    assert.deepEqual((await relay(onSolana, burnProof)).burned, [])
    const supply = await rpc('getNativeSupply')
    const vaultAfter = unpackAccount(pda(Buffer.from('vault'), mint.toBuffer()), await solana.getAccountInfo(pda(Buffer.from('vault'), mint.toBuffer()), 'finalized'), TOKEN_PROGRAM_ID).amount
    assert.equal(BigInt(supply.creditedLamports) - BigInt(supply.withdrawnLamports) - BigInt(supply.feeBurnedLamports), vaultAfter * scale, 'Credited less withdrawn and fee-burned equals the vault.')
    const crossed = (burnEpoch + 1) * epochSlots
    await until('The RPC follower matches past the fee-burn epoch', 60, async () => [...followerStates.keys()].some(slot => slot >= crossed))
    feeBurned = { epoch: burnEpoch, cblock: burnProof.consensusSlot, amount: amount.toString(), burnTransfers, burnTransactions: relayed.transactions, feeBurnedLamports: supply.feeBurnedLamports }
    event('native_lanes_fee_burned', feeBurned)
  }

  const silentWindow = await until('A future window draws the admitted outside member', 120, async () => {
    const latest = await acrux.getSlot('processed')
    return (await rpc('getSchedule', { limit: 8 })).find(window => window.firstSlot > latest + 8
      && window.firstSlot >= lanesFromSlot && window.proposers.some(proposer => proposer.identity === holder.publicKey.toBase58()))
  })
  const silentRole = silentWindow.proposers.find(proposer => proposer.identity === holder.publicKey.toBase58()).index
  await stopOwned(observer); observer = undefined
  await waitSlot(silentWindow.firstSlot + 16)
  const silent = await rpc('getSlot', { slot: silentWindow.firstSlot + 3 })
  const missing = silent.cblock.lanes.find(lane => lane.proposer === silentRole)
  assert.ok(missing && missing.status === 'skipped' && missing.certificates.includes('skip'))
  assert.ok(silent.cblock.lanes.filter(lane => lane.proposer !== silentRole).every(lane => lane.status === 'finalized'))
  assert.equal(silent.cblock.status, 'finalized')
  assert.ok(silent.cblock.finalizedUs !== null && silent.cblock.finalizedUs < silent.consensusTimeoutUs, 'The silent lane must be decided before the ordinary consensus skip timeout.')
  const silentAgreement = await until('All twelve hosted validators agree after the early silent-lane skip', 30,
    () => rpc('getExecutionAgreement', { slot: silent.slot }))
  assert.ok(silentAgreement.matching && silentAgreement.validators === 12)
  await until('The RPC follower keeps matching after the silent lane', 30, async () => followerStates.has(silent.slot))
  assert.equal(followerStates.get(silent.slot), silent.stateHash.slice(0, 12))
  let genesisStep
  if (genesisStepFromSlot !== undefined) {
    // A second outside member, offline, joins and stakes half a genesis weight; the first holds two.
    // Each counts up to one genesis weight, so the genesis falls by 1.5 weights shared by twelve.
    const stepEpoch = genesisStepFromSlot / epochSlots
    let handed = Math.floor(await acrux.getSlot('finalized') / epochSlots) + 1
    while (!(await onSolana.account.validatorSet.fetchNullable(addresses.set(handed), 'confirmed'))) handed--
    const relayThrough = async epoch => { for (; handed < epoch; handed++) await relayNext(handed) }
    const second = Keypair.generate(), secondStake = Keypair.generate(), secondLamports = genesisStakeLamports / 2n
    assert.ok(secondLamports >= BigInt(minimum.value), 'Half --genesis-stake-lamports must meet the Stake program minimum.')
    const secondMembership = rpda(Buffer.from('member'), second.publicKey.toBuffer())
    await send(acrux, [await registry.methods.joinHub().accountsStrict({
      payer: holder.publicKey, identity: second.publicKey, registry: registryAddress, hub, membership: secondMembership,
      systemProgram: SystemProgram.programId, eventAuthority: registryEvents, program: registry.programId,
    }).instruction()], holder, [second])
    const secondEffective = (await registry.account.membership.fetch(secondMembership)).effectiveEpoch.toNumber()
    assert.ok(secondEffective < stepEpoch, 'Choose a later genesis step: the second member must take effect before it.')
    const secondVote = PublicKey.findProgramAddressSync([Buffer.from('acrux_genesis_vote'), second.publicKey.toBuffer()], VoteProgram.programId)[0]
    await relayThrough(secondEffective)
    await waitSlot(secondEffective * epochSlots + 4)
    await until('The second member gets its Vote account', 30, async () => (await acrux.getAccountInfo(secondVote, 'finalized'))?.owner.equals(VoteProgram.programId))
    const secondCreate = StakeProgram.createAccount({ fromPubkey: holder.publicKey, stakePubkey: secondStake.publicKey,
      authorized: new Authorized(holder.publicKey, holder.publicKey), lamports: asNumber(secondLamports + BigInt(rent), 'second stake funding') })
    const secondDelegate = StakeProgram.delegate({ stakePubkey: secondStake.publicKey, authorizedPubkey: holder.publicKey, votePubkey: secondVote })
    const secondDelegated = await send(acrux, [...secondCreate.instructions, ...secondDelegate.instructions], holder, [secondStake])
    const secondReceipt = await acrux.getTransaction(secondDelegated, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
    assert.ok(secondReceipt.slot < (stepEpoch - 1) * epochSlots + epochSlots / 2, 'Choose a later genesis step: the second stake must be in the last unstepped set.')

    const genesisOf = set => set.members.filter(member => member.validatorKind === 'genesis')
    await relayThrough(stepEpoch)
    const unstepped = await rpc('getValidatorSet', { epoch: stepEpoch })
    assert.equal(unstepped.genesisWeight, null)
    assert.equal(unstepped.genesisStepFromSlot, genesisStepFromSlot)
    assert.ok(genesisOf(unstepped).length === 12 && genesisOf(unstepped).every(member => BigInt(member.stake) === genesisStakeLamports), 'Before the step the genesis keeps its whole weight.')
    assert.equal(BigInt(unstepped.members.find(member => member.identity === second.publicKey.toBase58()).stake), secondLamports)
    const stepped = (12n * genesisStakeLamports - genesisStakeLamports - secondLamports) / 12n
    await relayThrough(stepEpoch + 1)
    const first = await rpc('getValidatorSet', { epoch: stepEpoch + 1 })
    assert.equal(BigInt(first.genesisWeight), stepped)
    assert.ok(genesisOf(first).every(member => BigInt(member.stake) === stepped), 'The first stepped set lowers each genesis identity equally.')
    assert.equal(BigInt(first.members.find(member => member.identity === holder.publicKey.toBase58()).stake), stakeLamports, 'Outside stake keeps its full weight; only its count toward the step is capped.')
    const onSolanaStepped = await onSolana.account.validatorSet.fetch(addresses.set(stepEpoch + 1), 'confirmed')
    assert.ok(onSolanaStepped.validators.filter(member => genesisOf(first).some(g => g.identity === member.key.toBase58())).every(member => BigInt(member.stake.toString()) === stepped))

    // The second member leaves its stake; the next set drops it, and the genesis weight holds.
    await waitSlot((stepEpoch + 1) * epochSlots + 4)
    const deactivated = await send(acrux, StakeProgram.deactivate({ stakePubkey: secondStake.publicKey, authorizedPubkey: holder.publicKey }).instructions, holder)
    const deactivatedReceipt = await acrux.getTransaction(deactivated, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
    assert.ok(deactivatedReceipt.slot < (stepEpoch + 1) * epochSlots + epochSlots / 2, 'Use a longer local epoch: the undelegation must land before the cut-off.')
    // Set stepEpoch + 1 is the stepped one, so this relay is certified under the lowered weights.
    await relayThrough(stepEpoch + 2)
    const held = await rpc('getValidatorSet', { epoch: stepEpoch + 2 })
    assert.ok(!held.members.some(member => member.identity === second.publicKey.toBase58()))
    assert.equal(BigInt(held.genesisWeight), stepped, 'An outside undelegation never raises the genesis weight.')
    assert.ok(genesisOf(held).every(member => BigInt(member.stake) === stepped))
    await relayThrough(stepEpoch + 3)
    const crossed = (stepEpoch + 2) * epochSlots
    await until('The RPC follower matches past the stepped sets', 60, async () => [...followerStates.keys()].some(slot => slot >= crossed))
    const { createGenesisShareReader } = await import(new URL('../../api/dist/genesis-share.js', import.meta.url))
    const share = await createGenesisShareReader(gateway)()
    const rows = Object.fromEntries(share.epochs.map(row => [row.epoch, row]))
    assert.equal(share.genesisStepFromSlot, genesisStepFromSlot)
    assert.ok(rows[stepEpoch].genesisWeight === null && BigInt(rows[stepEpoch + 1].genesisWeight) === stepped)
    assert.ok(rows[stepEpoch + 1].genesisShareBps < rows[stepEpoch].genesisShareBps, '/v1/genesis-share reads the falling share.')
    genesisStep = { stepEpoch, genesisWeight: stepped.toString(), shareBps: [stepEpoch, stepEpoch + 1, stepEpoch + 2].map(epoch => rows[epoch]?.genesisShareBps),
      secondMember: second.publicKey.toBase58(), certifiedUnderStepped: stepEpoch + 2 }
    event('native_lanes_genesis_stepped', genesisStep)
  }
  event('native_lanes_check_passed', { identity: holder.publicKey.toBase58(), lanesFromSlot, epochSlots, before, after,
    boundaryBefore: boundaryBefore.agreement, boundaryAfter: boundaryAfter.agreement,
    withdrawal, withdrawalCblock: cblock, withdrawalSequence: withdrawalSequence.toNumber(), releasedBaseUnits: withdrawAmount.toString(),
    unchangedSolanaProgram: program.toBase58(), silentWindow: silentWindow.number, silentRole,
    silentFinalizedUs: silent.cblock.finalizedUs, skipTimeoutUs: silent.consensusTimeoutUs,
    rpcFollowerMatches: followerStates.size, observerMatches, nativeMint, feeBurned, genesisStep })
} catch (error) {
  console.error(`The local net's last output:\n${tail}\nOutside process:\n${observerTail}\nRPC follower:\n${followerTail}`)
  throw error
} finally {
  const failures = []
  for (const owned of [follower, observer, child]) {
    try { await stopOwned(owned) } catch (error) { failures.push(error) }
  }
  if (runDir && !failures.length) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must stay inside node/target.')
    rmSync(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
  if (failures.length) throw new AggregateError(failures, 'Check-owned process cleanup failed.')
}
