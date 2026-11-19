// Build 4 step 4 on a fresh operator-started local Solana validator at 4990, loaded with
// the new burn-capable bridge and funded local authority. No deploy, reset or validator stop.
// Acrux uses the EXISTING old/native placements; this check does not put the Solana upgrade
// on Acrux. It owns TCP5041..5043, QUIC5062..5073 and a temporary node/target store.
// With a private LOCAL-check ACRUX_IDENTITY_SEED set, run from backend/chain:
// node scripts/native-fee-burn-check.mjs --native-from-slot 8192 --epoch-slots 256 --native-scale 1000
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
import { Connection, Keypair, PublicKey, SystemProgram, Transaction } from '@solana/web3.js'
import { chainDir, DEV_SEED, devKeypair } from './dev-keys.mjs'
import { bridgeProgram, chainTag, decodeEntry, pdas, programData, relay, seed } from './bridge-relay.mjs'

const arg = name => { const i = process.argv.indexOf(`--${name}`); return i < 0 ? undefined : process.argv[i + 1] }
const fromRaw = arg('native-from-slot'), scaleRaw = arg('native-scale'), epochRaw = arg('epoch-slots')
if (!fromRaw || !scaleRaw || !epochRaw) throw new Error('Set --native-from-slot, --native-scale and --epoch-slots explicitly; see the usage example.')
const nativeFromSlot = Number(fromRaw), scale = BigInt(scaleRaw)
assert.ok(Number.isSafeInteger(nativeFromSlot) && nativeFromSlot > 0 && nativeFromSlot % 4 === 0, 'Local cut-over must be a positive four-slot boundary.')
const epochSlots = Number(epochRaw)
assert.ok(Number.isSafeInteger(epochSlots) && epochSlots > 0 && epochSlots % 16 === 0 && nativeFromSlot % epochSlots === 0, 'Use whole proposer windows and an epoch boundary for the local native cut-over.')
const nativeEpoch = nativeFromSlot / epochSlots, chainId = 'acrux-native-fee-burn-local-check'
assert.ok(scale > 0n && scale <= BigInt(Number.MAX_SAFE_INTEGER), 'Use a positive, exactly representable native scale.')
if (!process.env.ACRUX_IDENTITY_SEED?.trim() || process.env.ACRUX_IDENTITY_SEED.trim() === '20261116') throw new Error('Set a private local-check ACRUX_IDENTITY_SEED before running this check.')
const nodeDir = resolve(chainDir, '../node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const solanaUrl = 'http://127.0.0.1:4990', gateway = 'http://127.0.0.1:5041'
const fetchBounded = (input, init = {}) => fetch(input, { ...init, signal: init.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(10_000)]) : AbortSignal.timeout(10_000) })
const connection = url => new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true, fetch: fetchBounded })
const solana = connection(solanaUrl), acrux = connection(gateway)
const authority = devKeypair('local-authority'), holder = Keypair.generate(), mintKeypair = Keypair.generate()
const mint = mintKeypair.publicKey
const onSolana = bridgeProgram(solana, authority)
const program = onSolana.programId, addresses = pdas(program)
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const u64 = value => { const data = Buffer.alloc(8); data.writeBigUInt64LE(BigInt(value.toString())); return data }
const feeState = pda(Buffer.from('fee_burn'))
const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
const source = getAssociatedTokenAddressSync(mint, authority.publicKey)
const oldSource = getAssociatedTokenAddressSync(bridgedMint, holder.publicKey)
const depositAmount = 20_000_000n
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
let runDir, child, tail = '', interrupted = false, childError
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function ownNet() {
  if (interrupted) throw new Error('Native fee-burn check interrupted.')
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
// Keep the reference relay's instruction encoding, while using bounded HTTP confirmation here.
onSolana.provider.sendAndConfirm = (transaction, signers = []) => send(solana, transaction.instructions, authority, signers)
const token = async (chain, address) => {
  const info = await chain.getAccountInfo(address, 'finalized')
  return info ? unpackAccount(address, info, TOKEN_PROGRAM_ID) : null
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
async function deposit(amount) {
  const sequence = (await onSolana.account.bridge.fetch(addresses.bridge)).deposits
  const instruction = await onSolana.methods.deposit(new anchor.BN(amount.toString()), holder.publicKey).accountsStrict({
    depositor: authority.publicKey, bridge: addresses.bridge, mint, allowed: addresses.allowed(mint), source,
    vault: pda(Buffer.from('vault'), mint.toBuffer()), record: pda(Buffer.from('deposit'), u64(sequence)),
    tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    eventAuthority: pda(Buffer.from('__event_authority')), program,
  }).instruction()
  return send(solana, [instruction], authority)
}

async function waitSlot(slot) {
  const now = await acrux.getSlot('finalized')
  await until(`Finalized local slot ${slot}`, Math.ceil(Math.max(0, slot - now) / 20) + 120, async () => {
    const head = await rpc('getFinalizedCblock')
    return head.consensusSlot !== null && head.consensusSlot * 4 >= slot
  })
}

try {
  await freePorts()
  assert.ok(await solana.getAccountInfo(program, 'finalized'), 'Load the new burn-capable bridge on the local validator first.')
  assert.equal(await solana.getAccountInfo(addresses.bridge, 'finalized'), null, 'Use a fresh local validator; this check never resets it.')
  const mintRent = await solana.getMinimumBalanceForRentExemption(MINT_SIZE)
  await send(solana, [
    SystemProgram.createAccount({ fromPubkey: authority.publicKey, newAccountPubkey: mint, lamports: mintRent, space: MINT_SIZE, programId: TOKEN_PROGRAM_ID }),
    createInitializeMintInstruction(mint, 6, authority.publicKey, null, TOKEN_PROGRAM_ID),
    createAssociatedTokenAccountIdempotentInstruction(authority.publicKey, source, authority.publicKey, mint, TOKEN_PROGRAM_ID),
    createMintToInstruction(mint, source, authority.publicKey, depositAmount, [], TOKEN_PROGRAM_ID),
  ], authority, [mintKeypair])
  runDir = mkdtempSync(join(target, 'native-fee-burn-check-'))
  const rule = { program: program.toBase58(), fromSlot: 0, epochSlots, chainId, nativeFromSlot, nativeMint: mint.toBase58(), nativeScale: Number(scale), feeBurnFromSlot: nativeFromSlot }
  writeFileSync(join(runDir, 'bridge.json'), JSON.stringify(rule))
  mkdirSync(join(runDir, 'placements'))
  for (const name of ['acrux-bridge.json', 'acrux-bridge-native.json']) {
    const bytes = readFileSync(join(chainDir, 'placements', name))
    assert.equal(JSON.parse(bytes).accounts.at(-1).address, program.toBase58())
    writeFileSync(join(runDir, 'placements', name), bytes)
  }
  writeFileSync(join(runDir, 'placements/placements.json'), JSON.stringify({ placements: [
    { file: 'acrux-bridge.json', slot: 0 }, { file: 'acrux-bridge-native.json', slot: nativeFromSlot },
  ] }))
  const env = { ...process.env, ACRUX_GENESIS_SEED: DEV_SEED, ACRUX_BIND: '127.0.0.1', ACRUX_BRIDGE: join(runDir, 'bridge.json'), ACRUX_BRIDGE_RPC: `${solanaUrl},${solanaUrl}`, ACRUX_BRIDGE_PROOF_MAX: '100000', ACRUX_PLACEMENTS: join(runDir, 'placements/placements.json') }
  for (const name of ['ACRUX_STATE_WRITE_DELAY_MS', 'ACRUX_STATE_WRITE_FAIL_MS', 'ACRUX_LOG_RPC']) delete env[name]
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
  // The local authority seeds the first native epoch, so short check epochs need no earlier
  // Solana handoffs. Deposits can arrive before that future validator set is seeded.
  await send(solana, [
    await onSolana.methods.initialize(new anchor.BN(epochSlots), new anchor.BN(nativeEpoch), [...chainTag(chainId)]).accountsStrict({ payer: authority.publicKey, bridge: addresses.bridge, program, programData: programData(program), systemProgram: SystemProgram.programId }).instruction(),
    await onSolana.methods.allowMint().accountsStrict({ authority: authority.publicKey, bridge: addresses.bridge, mint, allowed: addresses.allowed(mint), systemProgram: SystemProgram.programId }).instruction(),
    await onSolana.methods.initFeeBurnConfig().accountsStrict({ authority: authority.publicKey, bridge: addresses.bridge, mint, config: addresses.feeBurnConfig, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId }).instruction(),
  ], authority)
  const deposited = await deposit(depositAmount)
  await until('Bridged tokens before the cut-over', 90, async () => (await token(acrux, oldSource))?.amount === depositAmount)
  assert.equal(await rpc('getNativeSupply'), null, 'Choose a later native cut-over if setup crossed it.')
  console.log(JSON.stringify({ event: 'native_fee_burn_deposited', deposited, mint: mint.toBase58(), nativeFromSlot, epochSlots, nativeEpoch, scale: scale.toString() }))
  await waitSlot(nativeFromSlot + 4)
  assert.equal(BigInt(await acrux.getBalance(holder.publicKey, 'finalized')), depositAmount * scale)
  const fees = (await rpc('getExecutionSnapshot')).executor.fees
  const destination = Keypair.generate().publicKey
  const signatures = [], receipts = []
  for (let i = 0; i < 3; i++) {
    const signature = await send(acrux, [SystemProgram.transfer({ fromPubkey: holder.publicKey, toPubkey: destination, lamports: 2_000_000 })], holder)
    const receipt = await acrux.getTransaction(signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
    assert.ok(receipt?.meta && !receipt.meta.err)
    assert.equal(Math.floor(receipt.slot / epochSlots), nativeEpoch, 'Choose a longer local epoch if the three transfers crossed its end.')
    signatures.push(signature); receipts.push(receipt)
  }
  const rawBurn = receipts.reduce((sum, receipt) => sum + BigInt(fees.baseFeePerSignature) * BigInt(receipt.transaction.message.header.numRequiredSignatures) * BigInt(fees.baseBurnBps) / 10_000n, 0n)
  assert.ok(rawBurn / scale > 0n, 'The chosen scale must let these local base fees burn at least one Solana base unit.')
  await waitSlot((nativeEpoch + 1) * epochSlots + 4)
  // The epoch's last cblock is final a few cblocks after the boundary; ask until it is.
  const proof = await until('The epoch-ending cblock proof', 60, () => rpc('getCblockProof', { feeBurnEpoch: nativeEpoch }))
  assert.ok(proof, 'The last cblock proof must survive until the epoch can be relayed.')
  const feeBurns = proof.outbox.map(bytes => decodeEntry(Buffer.from(bytes, 'base64'))).filter(entry => entry.feeBurn)
  assert.equal(feeBurns.length, 1)
  const feeBurn = feeBurns[0]
  assert.equal(feeBurn.epoch.toNumber(), nativeEpoch)
  assert.equal(BigInt(feeBurn.amount.toString()), rawBurn / scale)
  const marker = await acrux.getAccountInfo(feeState, 'finalized')
  assert.ok(marker && marker.owner.equals(program) && marker.data.length === 72)
  const rawTotal = marker.data.readBigUInt64LE(32), carry = marker.data.readBigUInt64LE(48)
  assert.equal(rawTotal, rawBurn)
  assert.equal(carry, rawBurn % scale)
  const vault = pda(Buffer.from('vault'), mint.toBuffer())
  const supplyBefore = BigInt((await solana.getTokenSupply(mint, 'finalized')).value.amount)
  const vaultBefore = (await token(solana, vault)).amount
  await seed(onSolana, proof, epochSlots)
  const relayed = await relay(onSolana, proof)
  assert.deepEqual(relayed.burned, [{ epoch: nativeEpoch, amount: feeBurn.amount.toString() }])
  const supplyAfter = BigInt((await solana.getTokenSupply(mint, 'finalized')).value.amount)
  const vaultAfter = (await token(solana, vault)).amount
  assert.equal(supplyBefore - supplyAfter, rawBurn / scale)
  assert.equal(vaultBefore - vaultAfter, rawBurn / scale)
  assert.deepEqual((await relay(onSolana, proof)).burned, [])
  assert.equal(BigInt((await solana.getTokenSupply(mint, 'finalized')).value.amount), supplyAfter)
  const supply = await rpc('getNativeSupply')
  assert.equal(BigInt(supply.feeBurnedLamports), rawBurn - carry)
  // Every deposit, withdrawal and FeeBurn is settled here: all in-flight adjustments are zero.
  assert.equal(BigInt(supply.creditedLamports) - BigInt(supply.withdrawnLamports) - BigInt(supply.feeBurnedLamports), vaultAfter * scale)
  assert.equal(BigInt(supply.circulatingLamports) + carry, vaultAfter * scale)
  console.log(JSON.stringify({ event: 'native_fee_burn_check_passed', deposited, signatures, epoch: nativeEpoch, cblock: proof.consensusSlot, burnTransactions: relayed.transactions, rawFeeBurnedLamports: rawBurn.toString(), carryLamports: carry.toString(), burnedBaseUnits: feeBurn.amount.toString(), mintSupplyBefore: supplyBefore.toString(), mintSupplyAfter: supplyAfter.toString(), vaultBefore: vaultBefore.toString(), vaultAfter: vaultAfter.toString(), nativeSupply: supply }))
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
