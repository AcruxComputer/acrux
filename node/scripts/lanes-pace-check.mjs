// R23: the provided pace check's 15-second warmup and 60-second measurement, with lanes active.
// Usage from backend, after the coordinated release build: node node/scripts/lanes-pace-check.mjs
// The twelve real hosted validators run normal default traffic. Only the empty Solana deposit
// source is mocked; no tokens, native conversion or admission are involved.
// Owns TCP 5041..5044, QUIC 5062..5074 and one temporary node/target directory; all are cleaned.
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import dgram from 'node:dgram'
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import http from 'node:http'
import { createRequire } from 'node:module'
import net from 'node:net'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const backend = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const nodeDir = join(backend, 'node'), target = join(nodeDir, 'target'), chainDir = join(backend, 'chain')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const reportPath = join(nodeDir, 'reports/r23-lanes-pace.json')
const require = createRequire(join(chainDir, 'package.json'))
const { PublicKey } = require('@solana/web3.js')
const gateway = 'http://127.0.0.1:5041', epochSlots = 432_000
const chainId = 'acrux-r23-lanes-pace-local'
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
let runDir, child, mock, tail = '', childError, mockError, interrupted = false
const providerReads = { left: 0, right: 0 }
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function healthy() {
  if (interrupted) throw new Error('Lanes pace check interrupted.')
  if (childError) throw childError
  if (mockError) throw mockError
  if (child && (child.exitCode !== null || child.signalCode !== null)) throw new Error(`The check-owned net exited: ${tail.slice(-8000)}`)
}
async function waitFor(ms) {
  const end = Date.now() + ms
  while (Date.now() < end) { healthy(); await pause(Math.min(1000, end - Date.now())) }
  healthy()
}
async function rpc(method, params = {}) {
  healthy()
  const response = await fetch(gateway, { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }), signal: AbortSignal.timeout(10_000) })
  assert.ok(response.ok, `${method}: HTTP ${response.status}`)
  const body = await response.json()
  assert.equal(body.error, undefined, `${method}: ${JSON.stringify(body.error)}`)
  return body.result
}
async function freePorts() {
  for (const port of [5041, 5042, 5043, 5044]) {
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
async function emptySolana(program) {
  const [bridge, bump] = PublicKey.findProgramAddressSync([Buffer.from('bridge')], program)
  // Unchanged Anchor Bridge: discriminator, deposits, bump, authority, epochSlots, firstEpoch,
  // seeded, chainId. deposits=0 means no deposit records or mint accounts may be requested.
  const data = Buffer.alloc(98)
  createHash('sha256').update('account:Bridge').digest().subarray(0, 8).copy(data)
  data[16] = bump
  program.toBuffer().copy(data, 17)
  data.writeBigUInt64LE(BigInt(epochSlots), 49)
  createHash('sha256').update(chainId).digest().copy(data, 66)
  const account = { owner: program.toBase58(), executable: false, lamports: 1_572_960,
    rentEpoch: 0, data: [data.toString('base64'), 'base64'], space: data.length }
  mock = http.createServer(async (request, response) => {
    let body
    try {
      assert.equal(request.method, 'POST')
      const provider = request.url === '/left' ? 'left' : request.url === '/right' ? 'right' : undefined
      assert.ok(provider, 'The empty Solana source has exactly two provider URLs.')
      const chunks = []; let bytes = 0
      for await (const chunk of request) { bytes += chunk.length; assert.ok(bytes <= 65_536); chunks.push(chunk) }
      body = JSON.parse(Buffer.concat(chunks).toString('utf8'))
      const { method, params } = body
      let value
      if (method === 'getAccountInfo') {
        assert.equal(params[0], bridge.toBase58(), 'No account except the empty Bridge should be read.')
        assert.equal(params[1].commitment, 'finalized')
        providerReads[provider]++
        value = account
      } else if (method === 'getMultipleAccounts') {
        assert.ok(Array.isArray(params[0]) && params[0].length === 0, 'This check has no withdrawals or release buckets.')
        value = []
      } else throw new Error(`Unexpected empty Solana RPC method: ${method}`)
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result: { context: { slot: 1 }, value } }))
    } catch (error) {
      mockError = error
      response.writeHead(400, { 'content-type': 'application/json' })
      response.end(JSON.stringify({ jsonrpc: '2.0', id: body?.id ?? null, error: { code: -32600, message: 'Unexpected empty Solana request.' } }))
    }
  })
  mock.once('error', error => { mockError = error })
  await new Promise((resolve, reject) => { mock.once('error', reject); mock.listen({ host: '127.0.0.1', port: 5044, exclusive: true }, resolve) })
}
async function stopOwned() {
  if (child?.pid && child.exitCode === null && child.signalCode === null) {
    const exited = new Promise(resolve => child.once('exit', resolve))
    child.kill('SIGTERM'); await Promise.race([exited, pause(5000)])
    if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await Promise.race([exited, pause(5000)]) }
    assert.ok(child.exitCode !== null || child.signalCode !== null, 'The check-owned net did not stop.')
  }
  if (mock?.listening) {
    mock.closeAllConnections()
    await new Promise((resolve, reject) => mock.close(error => error ? reject(error) : resolve()))
  }
}

try {
  assert.equal(process.argv.length, 2, 'This check uses one fixed 15-second warmup and 60-second measurement.')
  await access(binary)
  await freePorts()
  runDir = await mkdtemp(join(target, 'r23-lanes-pace-'))
  await mkdir(join(runDir, 'genesis'))
  const bridgeIdl = JSON.parse(await readFile(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8'))
  const program = new PublicKey(bridgeIdl.address)
  const rulePath = join(runDir, 'bridge.json')
  await writeFile(rulePath, JSON.stringify({ program: program.toBase58(), fromSlot: 0, epochSlots, chainId, lanesFromSlot: 0 }))
  const env = { ...process.env, ACRUX_BIND: '127.0.0.1', ACRUX_IDENTITY_SEED: randomBytes(32).toString('hex'),
    ACRUX_GENESIS_SEED: 'acrux-dev-seed-v1', ACRUX_BRIDGE: rulePath, ACRUX_PLACEMENTS: join(runDir, 'absent-placements.json'),
    ACRUX_BRIDGE_RPC: 'http://127.0.0.1:5044/left,http://127.0.0.1:5044/right', ACRUX_BRIDGE_PROOF_MAX: '100000',
    ACRUX_BRIDGE_PROOF_GRACE_SLOTS: '1728000' }
  for (const name of ['ACRUX_TRAFFIC_RATE', 'ACRUX_STATE_WRITE_DELAY_MS', 'ACRUX_STATE_WRITE_FAIL_MS', 'ACRUX_LOG_RPC']) delete env[name]
  const genesisPath = join(runDir, 'genesis/acrux-net.json')
  const generated = spawnSync(process.execPath, ['scripts/genesis.mjs', genesisPath],
    { cwd: chainDir, env, windowsHide: true, encoding: 'utf8', timeout: 30_000 })
  assert.equal(generated.status, 0, `Generate the private-identity local genesis: ${generated.stderr}`)
  const genesis = JSON.parse(await readFile(genesisPath, 'utf8'))
  for (const entry of genesis.programs) entry.path = resolve(chainDir, entry.path)
  await writeFile(genesisPath, JSON.stringify(genesis))
  await emptySolana(program)
  child = spawn(binary, ['net', 'up', '--genesis', genesisPath, '--store', join(runDir, 'net'),
    '--gateway-port', '5041', '--ws-port', '5042', '--metrics-port', '5043', '--base-port', '5062',
    '--peer-port', '5074', '--peer-address', '127.0.0.1:5074'],
    { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  child.once('error', error => { childError = error })
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-20_000) })
  console.log(JSON.stringify({ event: 'lanes_pace_started', warmupSeconds: 15, measurementSeconds: 60, defaultTrafficPerSecond: 50, lanesFromSlot: 0 }))
  await waitFor(15_000)
  const firstSlot = await rpc('getSlot', []), started = Date.now()
  assert.ok(Number.isSafeInteger(firstSlot) && firstSlot > 0)
  await waitFor(60_000)
  const lastSlot = await rpc('getSlot', []), measuredMs = Date.now() - started
  assert.ok(Number.isSafeInteger(lastSlot) && lastSlot > firstSlot, 'The lanes net must continue executing slots.')
  const head = await rpc('getFinalizedCblock')
  assert.ok(Number.isSafeInteger(head.consensusSlot) && head.consensusSlot > 0)
  const view = await rpc('getSlot', { slot: head.consensusSlot * 4 + 3 })
  assert.equal(view.cblock.status, 'finalized')
  assert.equal(view.cblock.lanes.length, 3, 'The measured net must actually use lanes, rather than silently taking the old path.')
  assert.ok(view.cblock.lanes.every(lane => ['finalized', 'skipped'].includes(lane.status)), 'Every lane in the finalized cblock must be decided.')
  assert.ok(providerReads.left > 0 && providerReads.right > 0, 'Both empty finalized deposit sources must be read.')
  healthy()
  const report = { check: 'r23-lanes-pace', binary: relative(backend, binary).split(sep).join('/'),
    binarySha256: createHash('sha256').update(await readFile(binary)).digest('hex'),
    measuredAt: new Date().toISOString(), warmupSeconds: 15, measurementSeconds: 60, measuredMs,
    firstSlot, lastSlot, slotsPerSecond: ((lastSlot - firstSlot) * 1000 / measuredMs).toFixed(2),
    defaultTrafficPerSecond: 50, lanesFromSlot: 0, fromSlot: 0, epochSlots,
    emptySolanaProviderReads: providerReads, finalizedCblock: view.cblock.consensusSlot, lanes: view.cblock.lanes }
  await mkdir(dirname(reportPath), { recursive: true })
  await writeFile(reportPath, JSON.stringify(report, null, 2) + '\n')
  console.log(JSON.stringify({ event: 'lanes_pace_passed', ...report, report: relative(backend, reportPath).split(sep).join('/') }))
} catch (error) {
  console.error(`The check-owned net's last output:\n${tail}`)
  throw error
} finally {
  await stopOwned()
  if (runDir) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must remain inside node/target.')
    await rm(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
