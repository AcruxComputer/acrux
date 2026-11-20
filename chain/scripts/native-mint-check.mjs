// SPL Token's native mint placed from `nativeMintFromSlot`: before it a wrapped account at
// So11111111111111111111111111111111111111112 cannot be created; from it, stock @solana/spl-token
// creates the associated account, wraps lamports with syncNative and unwraps with closeAccount,
// while an RPC follower re-executes across the slot with matching state hashes.
// From backend/chain, after the release build: node scripts/native-mint-check.mjs
// Owns TCP 5041..5044, QUIC 5062..5074, and one store under node/target.
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import http from 'node:http'
import { dirname, join, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { NATIVE_MINT, createAssociatedTokenAccountIdempotentInstruction, createCloseAccountInstruction, createSyncNativeInstruction, getAccount, getAssociatedTokenAddressSync, getMint } from '@solana/spl-token'

const chainDir = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const target = resolve(chainDir, '../node/target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const gateway = 'http://127.0.0.1:5041', epochSlots = 432_000, nativeMintFromSlot = 512
const chainId = 'acrux-native-mint-check-local'
const pause = ms => new Promise(r => setTimeout(r, ms))
let runDir, child, follower, mock, tail = '', followerTail = '', followerLine = '', failure
const followerStates = new Map()

function healthy() {
  if (failure) throw failure
  for (const [name, process, log] of [['net', child, tail], ['follower', follower, followerTail]]) {
    if (process && (process.exitCode !== null || process.signalCode !== null)) throw new Error(`The ${name} exited: ${log.slice(-6000)}`)
  }
}
async function until(label, seconds, read) {
  const end = Date.now() + seconds * 1000
  while (Date.now() < end) { healthy(); const value = await read().catch(() => undefined); if (value) return value; await pause(250) }
  throw new Error(`${label}: deadline exceeded after ${seconds}s`)
}
async function rpc(method, params) {
  const response = await fetch(gateway, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })
  const body = await response.json()
  if (body.error) throw new Error(JSON.stringify(body.error))
  return body.result
}
function emptySolana(program) {
  // The unchanged Bridge account with no deposits, read by the net's finalized deposit source.
  const [bridge, bump] = PublicKey.findProgramAddressSync([Buffer.from('bridge')], program)
  const data = Buffer.alloc(98)
  createHash('sha256').update('account:Bridge').digest().subarray(0, 8).copy(data)
  data[16] = bump
  program.toBuffer().copy(data, 17)
  data.writeBigUInt64LE(BigInt(epochSlots), 49)
  createHash('sha256').update(chainId).digest().copy(data, 66)
  const account = { owner: program.toBase58(), executable: false, lamports: 1_572_960, rentEpoch: 0, data: [data.toString('base64'), 'base64'], space: data.length }
  mock = http.createServer(async (request, response) => {
    const chunks = []
    for await (const chunk of request) chunks.push(chunk)
    const { id, method, params } = JSON.parse(Buffer.concat(chunks).toString('utf8'))
    const value = method === 'getAccountInfo' && params[0] === bridge.toBase58() ? account : method === 'getMultipleAccounts' ? [] : null
    response.writeHead(200, { 'content-type': 'application/json' })
    response.end(JSON.stringify({ jsonrpc: '2.0', id, result: { context: { slot: 1 }, value } }))
  })
  return new Promise(resolve => mock.listen({ host: '127.0.0.1', port: 5044 }, resolve))
}
async function stop(process) {
  if (!process?.pid || process.exitCode !== null || process.signalCode !== null) return
  const exited = new Promise(resolve => process.once('exit', resolve))
  process.kill('SIGTERM'); await Promise.race([exited, pause(5000)])
  if (process.exitCode === null && process.signalCode === null) { process.kill('SIGKILL'); await Promise.race([exited, pause(5000)]) }
}

try {
  await access(binary)
  runDir = await mkdtemp(join(target, 'native-mint-check-'))
  await mkdir(join(runDir, 'genesis'))
  const program = new PublicKey(JSON.parse(await readFile(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8')).address)
  const rulePath = join(runDir, 'bridge.json')
  await writeFile(rulePath, JSON.stringify({ program: program.toBase58(), fromSlot: 0, epochSlots, chainId, nativeMintFromSlot }))
  const env = { ...process.env, ACRUX_BIND: '127.0.0.1', ACRUX_IDENTITY_SEED: randomBytes(32).toString('hex'), ACRUX_GENESIS_SEED: 'acrux-dev-seed-v1',
    ACRUX_BRIDGE: rulePath, ACRUX_PLACEMENTS: join(runDir, 'absent-placements.json'),
    ACRUX_BRIDGE_RPC: 'http://127.0.0.1:5044/left,http://127.0.0.1:5044/right', ACRUX_BRIDGE_PROOF_MAX: '100000', ACRUX_BRIDGE_PROOF_GRACE_SLOTS: '1728000' }
  for (const name of ['ACRUX_TRAFFIC_RATE', 'ACRUX_LOG_RPC']) delete env[name]
  const genesisPath = join(runDir, 'genesis/acrux-net.json')
  const generated = spawnSync(process.execPath, ['scripts/genesis.mjs', genesisPath], { cwd: chainDir, env, windowsHide: true, encoding: 'utf8', timeout: 30_000 })
  assert.equal(generated.status, 0, `Generate the local genesis: ${generated.stderr}`)
  const genesis = JSON.parse(await readFile(genesisPath, 'utf8'))
  for (const entry of genesis.programs) entry.path = resolve(chainDir, entry.path)
  await writeFile(genesisPath, JSON.stringify(genesis))
  await emptySolana(program)
  child = spawn(binary, ['net', 'up', '--genesis', genesisPath, '--store', join(runDir, 'net'), '--gateway-port', '5041', '--ws-port', '5042',
    '--metrics-port', '5043', '--base-port', '5062', '--peer-port', '5074', '--peer-address', '127.0.0.1:5074'], { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-20_000) })
  child.once('error', error => { failure = error })
  await until('The local net answers', 60, async () => (await rpc('getSlot', [])) > 8)

  follower = spawn(binary, ['node', 'follow', gateway, '--genesis', genesisPath], { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  follower.once('error', error => { failure = error })
  follower.stderr.on('data', data => { followerTail = (followerTail + data).slice(-20_000) })
  follower.stdout.on('data', data => {
    followerTail = (followerTail + data).slice(-20_000); followerLine += data
    let newline
    while ((newline = followerLine.indexOf('\n')) >= 0) {
      const line = followerLine.slice(0, newline); followerLine = followerLine.slice(newline + 1)
      const matched = /^slot (\d+) re-executed.* state (\S+) matches\b/.exec(line)
      if (matched) followerStates.set(Number(matched[1]), matched[2])
      else if (/^slot \d+ re-executed\b/.test(line)) failure = new Error(`Follower mismatch: ${line}`)
    }
  })

  // Solana's PubSub answers on the JSON-RPC port's own WebSocket upgrade.
  const connection = new Connection(gateway, { commitment: 'confirmed', wsEndpoint: gateway.replace('http', 'ws') })
  const { devKeypair } = await import('./dev-keys.mjs')
  const faucet = devKeypair('faucet'), holder = Keypair.generate()
  await sendAndConfirmTransaction(connection, new Transaction().add(SystemProgram.transfer({ fromPubkey: faucet.publicKey, toPubkey: holder.publicKey, lamports: 3 * LAMPORTS_PER_SOL })), [faucet])
  const ata = getAssociatedTokenAddressSync(NATIVE_MINT, holder.publicKey)
  const wrap = () => new Transaction().add(
    createAssociatedTokenAccountIdempotentInstruction(holder.publicKey, ata, holder.publicKey, NATIVE_MINT),
    SystemProgram.transfer({ fromPubkey: holder.publicKey, toPubkey: ata, lamports: LAMPORTS_PER_SOL }),
    createSyncNativeInstruction(ata))

  // Before the slot: no native mint, and the stock wrap fails in the associated-account program.
  const before = await rpc('getSlot', [])
  assert.ok(before < nativeMintFromSlot - 40, `The check must reach the net before slot ${nativeMintFromSlot}; it is at ${before}.`)
  assert.equal(await connection.getAccountInfo(NATIVE_MINT), null)
  const refused = await sendAndConfirmTransaction(connection, wrap(), [holder]).then(() => null, error => error.message)
  assert.ok(refused && /IncorrectProgramId|incorrect program id/i.test(refused), `Wrapping before the slot must fail: ${refused}`)

  await until(`Slot ${nativeMintFromSlot + 8}`, 120, async () => (await rpc('getSlot', [])) >= nativeMintFromSlot + 8)
  const mint = await getMint(connection, NATIVE_MINT)
  assert.equal(mint.decimals, 9); assert.equal(mint.supply, 0n); assert.equal(mint.isInitialized, true)
  assert.equal(mint.mintAuthority, null); assert.equal(mint.freezeAuthority, null)
  const wrapped = await sendAndConfirmTransaction(connection, wrap(), [holder])
  const account = await getAccount(connection, ata)
  assert.equal(account.amount, BigInt(LAMPORTS_PER_SOL)); assert.equal(account.isNative, true)
  const balanceWrapped = await connection.getBalance(holder.publicKey)
  const unwrapped = await sendAndConfirmTransaction(connection, new Transaction().add(createCloseAccountInstruction(ata, holder.publicKey, holder.publicKey)), [holder])
  assert.equal(await connection.getAccountInfo(ata), null)
  const balanceAfter = await connection.getBalance(holder.publicKey)
  assert.ok(balanceAfter > balanceWrapped + LAMPORTS_PER_SOL, 'Closing the wrapped account returns its lamports and rent.')

  const crossed = await until('The follower matches past the slot', 120, async () => {
    const past = [...followerStates.keys()].filter(slot => slot >= nativeMintFromSlot + 16)
    return [...followerStates.keys()].some(slot => slot < nativeMintFromSlot) && past.length >= 8 ? past.length : false
  })
  console.log(JSON.stringify({ event: 'native_mint_check_passed', nativeMintFromSlot, refusedBefore: refused.match(/IncorrectProgramId|incorrect program id/i)[0],
    mint: { decimals: mint.decimals, supply: mint.supply.toString() }, wrapped, unwrapped, wrappedAmount: account.amount.toString(),
    followerMatched: followerStates.size, followerPastSlot: crossed }))
} catch (error) {
  console.error(`The net's last output:\n${tail.slice(-4000)}\nThe follower's:\n${followerTail.slice(-2000)}`)
  throw error
} finally {
  await stop(follower); await stop(child)
  if (mock?.listening) { mock.closeAllConnections(); await new Promise(resolve => mock.close(resolve)) }
  if (runDir) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must remain inside node/target.')
    await rm(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
// web3.js keeps its subscription socket open; the check is done once its stores are gone.
process.exit(process.exitCode ?? 0)
