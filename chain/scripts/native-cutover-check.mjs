// Build 4 step 1 on a local stand. The operator first rebuilds acrux_bridge.so and
// starts a fresh local validator on 4990 with that program loaded and the local authority funded.
// This script never deploys, upgrades, resets or stops that validator. It starts its own Acrux
// net on 5041/5042/5043 and QUIC 5062..5073 (a Windows service holds UDP 5050), with a unique temporary store, then removes it.
// Acrux starts with the immutable existing bridge placement; the rebuilt native .so replaces it
// only at the configured cut-over. It deposits before and after that future cut-over, checks conversion/backing and the retired
// faucet, withdraws native ACRUX, and uses the existing manual relay helper for the same
// Withdrawal record the production keeper consumes. The recipient pays for its Solana ATA.
// Usage from backend/chain (ACRUX_IDENTITY_SEED must be a private LOCAL-check seed):
// node scripts/native-cutover-check.mjs --native-from-slot 8192 --native-scale 1000
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
import { Connection, Keypair, PublicKey, SystemProgram, Transaction, TransactionMessage, VersionedTransaction } from '@solana/web3.js'
import { chainDir, DEV_SEED, devKeypair } from './dev-keys.mjs'
import { bridgeProgram, pdas, relay, seed } from './bridge-relay.mjs'

const arg = name => { const i = process.argv.indexOf(`--${name}`); return i < 0 ? undefined : process.argv[i + 1] }
const fromRaw = arg('native-from-slot'), scaleRaw = arg('native-scale')
if (!fromRaw || !scaleRaw) throw new Error('Set --native-from-slot and --native-scale explicitly; see the usage example.')
const nativeFromSlot = Number(fromRaw), scale = BigInt(scaleRaw)
assert.ok(Number.isSafeInteger(nativeFromSlot) && nativeFromSlot > 0 && nativeFromSlot % 4 === 0, 'Local cut-over must be a positive four-slot boundary.')
// One local bridge epoch ends at the chosen cut-over: the first relay seeds the set that
// certifies its native withdrawal. The example leaves time for local Solana finality.
const epochSlots = nativeFromSlot
assert.ok(scale > 0n && scale <= BigInt(Number.MAX_SAFE_INTEGER), 'Use a positive, exactly representable native scale.')
if (!process.env.ACRUX_IDENTITY_SEED?.trim() || process.env.ACRUX_IDENTITY_SEED.trim() === '20261116') throw new Error('Set a private local-check ACRUX_IDENTITY_SEED before running this check.')
const nodeDir = resolve(chainDir, '../node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const solanaUrl = 'http://127.0.0.1:4990', gateway = 'http://127.0.0.1:5041', internal = 'http://127.0.0.1:5043'
const fetchBounded = (input, init = {}) => fetch(input, { ...init, signal: init.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(10_000)]) : AbortSignal.timeout(10_000) })
const connection = url => new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true, fetch: fetchBounded })
const solana = connection(solanaUrl), acrux = connection(gateway)
const authority = devKeypair('local-authority'), holder = Keypair.generate(), mintKeypair = Keypair.generate()
const mint = mintKeypair.publicKey
const onSolana = bridgeProgram(solana, authority), onAcrux = bridgeProgram(acrux, holder)
const program = onSolana.programId, addresses = pdas(program)
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program)[0]
const u64 = value => { const data = Buffer.alloc(8); data.writeBigUInt64LE(BigInt(value.toString())); return data }
const native = pda(Buffer.from('native')), burn = pda(Buffer.from('native_burn'))
const minimum = pda(Buffer.from('minimum'), mint.toBuffer()), outbound = pda(Buffer.from('outbound'))
const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
const source = getAssociatedTokenAddressSync(mint, authority.publicKey)
const oldSource = getAssociatedTokenAddressSync(bridgedMint, holder.publicKey)
const recipient = holder.publicKey, destination = getAssociatedTokenAddressSync(mint, recipient)
const beforeAmount = 20_000_000n, afterAmount = 5_000_000n, withdrawAmount = 3_000_000n
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
let runDir, child, tail = '', interrupted = false, childError
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function ownNet() {
  if (interrupted) throw new Error('Native cut-over check interrupted.')
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

try {
  await freePorts()
  assert.ok(await solana.getAccountInfo(program, 'finalized'), 'Load the rebuilt local bridge program before this check.')
  assert.equal(await solana.getAccountInfo(addresses.bridge, 'finalized'), null, 'Use a fresh local validator with an uninitialized bridge; this check never resets it.')
  assert.equal(await solana.getAccountInfo(native, 'finalized'), null)
  // NativeGate is never installed by the Solana program, so the native instruction refuses there.
  const rejected = await onSolana.methods.withdrawNative(new anchor.BN(1), recipient).accountsStrict({ owner: authority.publicKey, native, minimum, burn, outbound, record: pda(Buffer.from('withdrawal'), u64(0)), systemProgram: SystemProgram.programId }).instruction()
  const latest = await solana.getLatestBlockhash('confirmed')
  const simulation = new VersionedTransaction(new TransactionMessage({ payerKey: authority.publicKey, recentBlockhash: latest.blockhash, instructions: [rejected] }).compileToV0Message())
  simulation.sign([authority])
  const refused = (await solana.simulateTransaction(simulation, { sigVerify: true, commitment: 'confirmed' })).value
  assert.ok(refused.err && refused.logs?.some(line => line.includes('Instruction: WithdrawNative')) && refused.logs.some(line => line.includes('AccountNotInitialized')), 'The rebuilt instruction must refuse Solana without an executor-installed native gate.')

  const mintRent = await solana.getMinimumBalanceForRentExemption(MINT_SIZE)
  await send(solana, [
    SystemProgram.createAccount({ fromPubkey: authority.publicKey, newAccountPubkey: mint, lamports: mintRent, space: MINT_SIZE, programId: TOKEN_PROGRAM_ID }),
    createInitializeMintInstruction(mint, 6, authority.publicKey, null, TOKEN_PROGRAM_ID),
    createAssociatedTokenAccountIdempotentInstruction(authority.publicKey, source, authority.publicKey, mint, TOKEN_PROGRAM_ID),
    createMintToInstruction(mint, source, authority.publicKey, beforeAmount + afterAmount, [], TOKEN_PROGRAM_ID),
    SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: holder.publicKey, lamports: 1_000_000_000 }),
  ], authority, [mintKeypair])
  runDir = mkdtempSync(join(target, 'native-cutover-check-'))
  const rule = { program: program.toBase58(), fromSlot: 0, epochSlots, chainId: 'acrux-native-cutover-local-check', nativeFromSlot, nativeMint: mint.toBase58(), nativeScale: Number(scale) }
  writeFileSync(join(runDir, 'bridge.json'), JSON.stringify(rule))
  mkdirSync(join(runDir, 'placements'))
  const env = { ...process.env, ACRUX_GENESIS_SEED: DEV_SEED, ACRUX_BIND: '127.0.0.1', ACRUX_BRIDGE: join(runDir, 'bridge.json'), ACRUX_BRIDGE_RPC: `${solanaUrl},${solanaUrl}`, ACRUX_BRIDGE_PROOF_MAX: '100000', ACRUX_PLACEMENTS: join(runDir, 'placements/placements.json') }
  for (const name of ['ACRUX_STATE_WRITE_DELAY_MS', 'ACRUX_STATE_WRITE_FAIL_MS', 'ACRUX_LOG_RPC']) delete env[name]
  const oldPlacementBytes = readFileSync(join(chainDir, 'placements/acrux-bridge.json'))
  const oldPlacement = JSON.parse(oldPlacementBytes)
  assert.equal(oldPlacement.accounts.at(-1).address, program.toBase58(), 'The existing placement must use this bridge address.')
  writeFileSync(join(runDir, 'placements/acrux-bridge-before.json'), oldPlacementBytes)
  const replacementFile = join(runDir, 'placements/acrux-bridge-native.json')
  const placement = spawnSync(process.execPath, ['scripts/place-bridge.mjs', replacementFile], { cwd: chainDir, env, windowsHide: true, encoding: 'utf8', timeout: 30_000 })
  assert.equal(placement.status, 0, 'Prepare the Acrux placement from the operator-built native bridge .so.')
  const replacement = JSON.parse(readFileSync(replacementFile, 'utf8'))
  const programDataAddress = new PublicKey(oldPlacement.accounts[0].address)
  assert.equal(replacement.accounts[0].address, programDataAddress.toBase58())
  assert.notEqual(replacement.accounts[0].data, oldPlacement.accounts[0].data, 'Rebuild the native .so; the check must exercise different bytecode at the cut-over.')
  writeFileSync(join(runDir, 'placements/placements.json'), JSON.stringify({ placements: [
    { file: 'acrux-bridge-before.json', slot: 0 },
    { file: 'acrux-bridge-native.json', slot: nativeFromSlot },
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
  assert.ok(first.consensusSlot * 4 < nativeFromSlot, 'Choose a later local cut-over slot.')
  await seed(onSolana, first, epochSlots)
  await relay(onSolana, first) // Seeds epoch 1 from the working light-client helper.
  await send(solana, [await onSolana.methods.allowMint().accountsStrict({ authority: authority.publicKey, bridge: addresses.bridge, mint, allowed: addresses.allowed(mint), systemProgram: SystemProgram.programId }).instruction()], authority)
  const grant = await fetchBounded(`${internal}/internal/faucet`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ address: holder.publicKey.toBase58(), amountLamports: '2000000000' }) })
  assert.ok(grant.ok)
  await until('The holder pre-cut-over test balance', 30, async () => await acrux.getBalance(holder.publicKey, 'finalized') > 0)
  const beforeDeposit = await deposit(beforeAmount)
  await until('Bridged tokens before the cut-over', 90, async () => (await token(acrux, oldSource))?.amount === beforeAmount)
  assert.equal(await rpc('getNativeSupply'), null)
  assert.ok((await acrux.getAccountInfo(programDataAddress, 'finalized')).data.equals(Buffer.from(oldPlacement.accounts[0].data, 'base64')), 'Acrux must retain the original bridge bytecode before the cut-over.')
  console.log(JSON.stringify({ event: 'native_before_cutover', deposit: beforeDeposit, bridgedBaseUnits: beforeAmount.toString(), nativeFromSlot }))

  const converted = await until('The configured native cut-over', Math.ceil(nativeFromSlot / 20) + 120, async () => {
    const supply = await rpc('getNativeSupply')
    return supply && BigInt(supply.creditedLamports) === beforeAmount * scale ? supply : null
  })
  assert.equal(BigInt(converted.scale), scale)
  assert.equal(converted.mint, mint.toBase58())
  assert.equal(converted.cutoverSlot, nativeFromSlot)
  assert.ok((await acrux.getAccountInfo(programDataAddress, 'finalized')).data.equals(Buffer.from(replacement.accounts[0].data, 'base64')), 'The native bridge bytecode must be placed at the cut-over.')
  assert.equal((await token(acrux, oldSource)).amount, 0n)
  assert.equal((await acrux.getTokenSupply(bridgedMint, 'finalized')).value.amount, '0')
  assert.equal(BigInt(await acrux.getBalance(holder.publicKey, 'finalized')), beforeAmount * scale, 'The old test balance must be retired rather than added to the conversion.')
  assert.equal(BigInt(converted.circulatingLamports), beforeAmount * scale)
  const faucetAnswer = {}
  for (const method of ['GET', 'POST']) {
    const retired = await fetchBounded(`${internal}/internal/faucet`, method === 'GET' ? {} : { method, headers: { 'content-type': 'application/json' }, body: JSON.stringify({ address: holder.publicKey.toBase58(), amountLamports: '1' }) })
    assert.equal(retired.status, 410)
    const body = await retired.json()
    assert.equal(body.error.code, 'FAUCET_RETIRED')
    faucetAnswer[method.toLowerCase()] = { status: retired.status, body }
  }

  const afterDeposit = await deposit(afterAmount)
  await until('Native deposit after the cut-over', 90, async () => BigInt((await rpc('getNativeSupply')).creditedLamports) === (beforeAmount + afterAmount) * scale)
  assert.equal(BigInt(await acrux.getBalance(holder.publicKey, 'finalized')), (beforeAmount + afterAmount) * scale)
  assert.equal((await acrux.getTokenSupply(bridgedMint, 'finalized')).value.amount, '0')
  await send(solana, [createAssociatedTokenAccountIdempotentInstruction(holder.publicKey, destination, recipient, mint, TOKEN_PROGRAM_ID)], holder)
  const count = await onAcrux.account.outbound.fetchNullable(outbound)
  const sequence = count?.withdrawals ?? new anchor.BN(0)
  const record = pda(Buffer.from('withdrawal'), u64(sequence))
  const withdrawal = await onAcrux.methods.withdrawNative(new anchor.BN(withdrawAmount.toString()), recipient).accountsStrict({ owner: holder.publicKey, native, minimum, burn, outbound, record, systemProgram: SystemProgram.programId }).instruction()
  const signature = await send(acrux, [withdrawal], holder)
  const made = await onAcrux.account.withdrawal.fetch(record, 'finalized')
  assert.equal(made.amount.toString(), withdrawAmount.toString())
  assert.ok(made.solanaMint.equals(mint))
  const proof = await until('Native withdrawal proof', 60, () => rpc('getCblockProof', { consensusSlot: Math.floor(made.slot.toNumber() / 4) }))
  const relayed = await relay(onSolana, proof)
  assert.ok(relayed.released.some(entry => entry.sequence === sequence.toNumber()))
  assert.equal((await token(solana, destination)).amount, withdrawAmount)
  const receipt = await acrux.getTransaction(signature, { commitment: 'finalized', maxSupportedTransactionVersion: 0 })
  assert.ok(receipt?.meta && !receipt.meta.err)
  const snapshot = await rpc('getExecutionSnapshot')
  const fees = snapshot.executor.fees
  const baseFee = BigInt(fees.baseFeePerSignature) * BigInt(receipt.transaction.message.header.numRequiredSignatures)
  const localBaseBurn = baseFee * BigInt(fees.baseBurnBps) / 10_000n
  const supply = await rpc('getNativeSupply')
  const web3Supply = (await acrux.getSupply({ commitment: 'finalized', excludeNonCirculatingAccountsList: false })).value
  assert.equal(BigInt(web3Supply.nonCirculating), BigInt(supply.reserveLamports))
  assert.equal(BigInt(web3Supply.circulating), BigInt(supply.circulatingLamports))
  assert.equal(BigInt(web3Supply.total), BigInt(web3Supply.nonCirculating) + BigInt(web3Supply.circulating))
  const locked = (await token(solana, pda(Buffer.from('vault'), mint.toBuffer()))).amount
  assert.equal(BigInt(supply.creditedLamports), (beforeAmount + afterAmount) * scale)
  assert.equal(BigInt(supply.withdrawnLamports), withdrawAmount * scale)
  assert.equal(BigInt(supply.circulatingLamports) + localBaseBurn, locked * scale, 'Only burned base fees reduce native backing; rent and transferred fees stay in circulation.')
  console.log(JSON.stringify({ event: 'native_cutover_check_passed', nativeFromSlot, mint: mint.toBase58(), scale: scale.toString(), beforeDeposit, afterDeposit, withdrawal: signature, sequence: sequence.toNumber(), creditedLamports: supply.creditedLamports, withdrawnLamports: supply.withdrawnLamports, circulatingLamports: supply.circulatingLamports, reserveLamports: supply.reserveLamports, faucetAnswer, localBaseBurnLamports: localBaseBurn.toString(), lockedBaseUnits: locked.toString(), solanaPaidBaseUnits: withdrawAmount.toString() }))
} catch (error) {
  console.error(`The local net's last output:
${tail}`)
  throw error
} finally {
  if (child && child.exitCode === null && child.signalCode === null) {
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
