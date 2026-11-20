// R24: two fresh nets (today/lanes), each 15 s warmup + 60 s measurement, then 12 lab runs.
// Windows smoke: node node/scripts/lanes-handover-pace.mjs --binary node/target/release/acrux.exe
// Linux decision: docker run --rm --network none --mount type=bind,source=<absolute-backend>,target=/backend -w /backend node:22-bookworm-slim node node/scripts/lanes-handover-pace.mjs --binary /backend/dist-pack/acrux-linux-0.4.35/acrux
// No Solana validator; only an empty deposit source is mocked. Default traffic stays on.
// Allow 5-7 min. Owns TCP 5041..5044, QUIC 5062..5074 and one unique node/target directory.
// Both nets share bridge/domain/identity settings; only lanesFromSlot changes. Read slot views
// after each measured interval. Windows numbers are smoke evidence, not the Linux result.
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import dgram from 'node:dgram'
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import http from 'node:http'
import { createReadStream, createWriteStream } from 'node:fs'
import { createInterface } from 'node:readline'
import { pipeline } from 'node:stream/promises'
import { createGzip } from 'node:zlib'
import { createRequire } from 'node:module'
import net from 'node:net'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const backend = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const nodeDir = join(backend, 'node'), target = join(nodeDir, 'target'), chainDir = join(backend, 'chain')
assert.ok(process.argv.length === 4 && process.argv[2] === '--binary', 'Pass exactly --binary <path>; there is no default binary.')
const binary = resolve(process.argv[3])
assert.ok(process.platform === 'win32' || !binary.startsWith(resolve(target, 'release') + sep), 'Put the Linux binary outside node/target/release; use dist-pack/acrux-linux-0.4.35/acrux.')
const platform = process.platform === 'win32' ? 'windows' : process.platform
const reportPath = join(nodeDir, `reports/r24-handover-pace-${platform}.json`)
const require = createRequire(join(chainDir, 'package.json'))
const { Keypair, PublicKey } = require('@solana/web3.js')
const gateway = 'http://127.0.0.1:5041', epochSlots = 432_000
const chainId = 'acrux-r24-handover-pace-local'
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
let runDir, child, mock, tail = '', childError, mockError, interrupted = false
const providerReads = { left: 0, right: 0 }
process.once('SIGINT', () => { interrupted = true })
process.once('SIGTERM', () => { interrupted = true })
function healthy() {
  if (interrupted) throw new Error('Handover measurement interrupted.')
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
  const account = { owner: program.toBase58(), executable: false, lamports: (128 + data.length) * 3480 * 2,
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

const seeds = ['acrux-r20-agreement', 'acrux-r24-handover-2', 'acrux-r24-handover-3']
const event = (name, fields) => console.log(JSON.stringify({ event: name, ...fields }))
const report = { check: 'r24-handover-pace', platform, authoritativeLinuxMeasurement: process.platform === 'linux',
  interpretation: process.platform === 'linux' ? 'Linux measurements for the handover decision.' : 'Windows script smoke only; these are not the Linux performance result.',
  binary: relative(backend, binary).split(sep).join('/'), startedAt: new Date().toISOString(), seeds,
  defaultTrafficPerSecond: 50, netWarmupSeconds: 15, netMeasurementSeconds: 60, collectorReadIntervalMs: 50, labRunSeconds: 12,
  measurementDefinitions: {
    slotsPerSecond: 'Net: getSlot difference / measured wall duration after warmup. Lab: slot difference / slot.started wallTime difference, node zero, slots48..95.',
    handoverGapMs: 'Next-window first-slot wallTime minus previous-window last-slot wallTime; arithmetic median.',
    cblockFinalityUs: 'Cblock finalizedUs from its start, the Facts.cblockFinalizationUs anchor; nearest-rank p50/p90. Missing observations stay missing.',
  }, nets: [], labs: [], completed: false }
function percentile(values, share) {
  const ordered = [...values].sort((a, b) => a - b)
  return ordered.length ? ordered[Math.ceil(ordered.length * share) - 1] : null
}
function median(values) {
  const ordered = [...values].sort((a, b) => a - b), n = ordered.length
  return n ? (ordered[Math.floor((n - 1) / 2)] + ordered[Math.floor(n / 2)]) / 2 : null
}
function slotMetrics(slots) {
  const ordered = [...slots].sort((a, b) => a.slot - b.slot), first = ordered[0], last = ordered.at(-1)
  const missing = [], handovers = []
  for (let i = 1; i < ordered.length; i++) {
    const before = ordered[i - 1], after = ordered[i]
    if (after.slot !== before.slot + 1) missing.push([before.slot + 1, after.slot - 1])
    if (after.slot % 16 === 0 && before.slot + 1 === after.slot) handovers.push({ fromSlot: before.slot, toSlot: after.slot, gapMs: after.wallMs - before.wallMs })
  }
  const elapsedMs = first && last ? last.wallMs - first.wallMs : null
  return { firstSlot: first?.slot ?? null, lastSlot: last?.slot ?? null, observedSlots: ordered.length, elapsedMs,
    eventWallSlotsPerSecond: elapsedMs > 0 && !missing.length ? (last.slot - first.slot) * 1000 / elapsedMs : null,
    slotPaceAvailable: elapsedMs > 0 && !missing.length, missingSlotRanges: missing,
    handoverAvailable: handovers.length > 0, handoverMedianMs: median(handovers.map(row => row.gapMs)), handovers }
}
function finalityMetrics(rows) {
  const valid = rows.filter(row => row.status === 'finalized' && Number.isSafeInteger(row.finalizedUs) && row.finalizedUs >= 0)
  return { p50: percentile(valid.map(row => row.finalizedUs), 0.5), p90: percentile(valid.map(row => row.finalizedUs), 0.9),
    available: valid.length > 0, complete: rows.length > 0 && valid.length === rows.length, count: valid.length, missingCblocks: rows.filter(row => !valid.includes(row)).map(row => row.cblock), rows }
}
async function saveReport() {
  report.updatedAt = new Date().toISOString()
  await mkdir(dirname(reportPath), { recursive: true })
  await writeFile(reportPath, JSON.stringify(report, null, 2) + '\n')
}
function startChild(args, cwd, env) {
  tail = ''; childError = undefined; mockError = undefined
  child = spawn(binary, args, { cwd, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  child.once('error', error => { childError = error })
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-20_000) })
  return child
}
async function finishChild() {
  await stopOwned(); child = undefined; mock = undefined
}
async function measuredNet(protocol, genesis, env, program) {
  await freePorts()
  const dir = join(runDir, `net-${protocol}`); await mkdir(dir)
  const rule = { program: program.toBase58(), fromSlot: 0, epochSlots, chainId, ...(protocol === 'lanes' ? { lanesFromSlot: 0 } : {}) }
  const rulePath = join(dir, 'bridge.json'); await writeFile(rulePath, JSON.stringify(rule))
  providerReads.left = 0; providerReads.right = 0
  await emptySolana(program)
  event('handover_net_started', { protocol, warmupSeconds: 15, measurementSeconds: 60 })
  startChild(['net', 'up', '--genesis', genesis, '--store', join(dir, 'store'), '--gateway-port', '5041', '--ws-port', '5042',
    '--metrics-port', '5043', '--base-port', '5062', '--peer-port', '5074', '--peer-address', '127.0.0.1:5074'], dir, { ...env, ACRUX_BRIDGE: rulePath })
  try {
    await waitFor(15_000)
    const firstSlot = await rpc('getSlot', []), start = Date.now()
    assert.ok(Number.isSafeInteger(firstSlot) && firstSlot > 0)
    await waitFor(60_000)
    const lastSlot = await rpc('getSlot', []), measuredMs = Date.now() - start
    assert.ok(Number.isSafeInteger(lastSlot) && lastSlot > firstSlot)
    report.activeNetInterval = { protocol, measuredFirstSlot: firstSlot, measuredLastSlot: lastSlot, measuredMs,
      slotsPerSecond: (lastSlot - firstSlot) * 1000 / measuredMs }
    await saveReport()
    // Retrieving the retained views adds no load to the measured interval.
    const slots = [], missingSlotViews = []
    for (let from = firstSlot; from <= lastSlot;) {
      // Fixed post-measurement collection pace: 20 reads/s, below the gateway's default 50.
      await pause(50)
      const count = Math.min(64, lastSlot - from + 1), page = await rpc('getSlotRange', { from, count })
      assert.ok(page.length <= count)
      for (const [index, row] of page.entries()) {
        assert.equal(row.slot, from + index, 'A returned range must be consecutive up to its first absent view.')
        const wallMs = Date.parse(row.view.wallTime); assert.ok(Number.isFinite(wallMs))
        slots.push({ slot: row.slot, wallMs, wallTime: row.view.wallTime, cblock: row.view.cblock })
      }
      from += page.length
      if (page.length < count) { missingSlotViews.push(from); from++ }
    }
    const finals = new Map()
    for (let cblock = Math.ceil(firstSlot / 4); cblock * 4 + 3 <= lastSlot; cblock++) {
      finals.set(cblock, { cblock, status: 'view_missing', finalizedUs: null })
    }
    for (const row of slots) {
      const c = row.cblock, first = c.consensusSlot * 4
      if (first < firstSlot || first + 3 > lastSlot) continue
      const held = finals.get(c.consensusSlot)
      if (!held || held.status === 'view_missing' || c.finalizedUs !== null) finals.set(c.consensusSlot, { cblock: c.consensusSlot, status: c.status, finalizedUs: c.finalizedUs })
    }
    const decided = slots.filter(row => row.cblock.status === 'finalized').at(-1)?.cblock
    assert.ok(decided, 'The measured net must finalize cblocks.')
    if (protocol === 'lanes') {
      assert.equal(decided.lanes.length, 3)
      assert.ok(decided.lanes.every(lane => ['finalized', 'skipped'].includes(lane.status)))
    } else assert.equal(decided.lanes, undefined, 'The control must use the unchanged legacy path.')
    assert.ok(providerReads.left > 0 && providerReads.right > 0)
    const metrics = slotMetrics(slots)
    if (missingSlotViews.length) {
      metrics.eventWallSlotsPerSecond = null; metrics.slotPaceAvailable = false; metrics.missingSlotRanges = []
      for (const slot of missingSlotViews) {
        const range = metrics.missingSlotRanges.at(-1)
        if (range && range[1] + 1 === slot) range[1] = slot
        else metrics.missingSlotRanges.push([slot, slot])
      }
    }
    const result = { protocol, measuredMs, measuredFirstSlot: firstSlot, measuredLastSlot: lastSlot, slotsPerSecond: (lastSlot - firstSlot) * 1000 / measuredMs,
      ...metrics, missingSlotViews, finalityUs: finalityMetrics([...finals.values()]), emptySolanaProviderReads: { ...providerReads },
      actualFinalizedCblock: { consensusSlot: decided.consensusSlot, status: decided.status, ...(decided.lanes ? { lanes: decided.lanes } : {}) },
      slotStarts: slots.map(({ slot, wallMs, wallTime }) => ({ slot, wallMs, wallTime })) }
    event('handover_net_finished', { protocol, slotsPerSecond: result.slotsPerSecond, handoverMedianMs: result.handoverMedianMs,
      finalityUs: { p50: result.finalityUs.p50, p90: result.finalityUs.p90, count: result.finalityUs.count, missing: result.finalityUs.missingCblocks.length } })
    return result
  } finally { await finishChild(); await rm(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }) }
}

async function measuredLab(scenario, protocol, seed, seedIndex, genesis, env, ownIdentity) {
  if (interrupted) throw new Error('Handover measurement interrupted.')
  await freePorts()
  const dir = join(runDir, `lab-${scenario}-${protocol}-${seedIndex}`); await mkdir(dir)
  event('handover_lab_started', { scenario, protocol, seed })
  const processRun = startChild(['lab', 'run', '--scenario', scenario, '--protocol', protocol, '--seed', seed,
    '--base-port', '5062', '--out', dir, '--genesis', genesis], dir, env)
  let timedOut = false
  const soft = setTimeout(() => { timedOut = true; processRun.kill('SIGTERM') }, 45_000)
  const hard = setTimeout(() => { if (processRun.exitCode === null && processRun.signalCode === null) processRun.kill('SIGKILL') }, 50_000)
  let exit
  try { exit = await new Promise(resolve => processRun.once('close', (exitCode, signal) => resolve({ exitCode, signal }))) }
  finally { clearTimeout(soft); clearTimeout(hard) }
  child = undefined
  const outcome = JSON.parse(await readFile(join(dir, 'outcome.json'), 'utf8'))
  const starts = new Map()
  const input = createReadStream(join(dir, 'events.jsonl'), { encoding: 'utf8' }), lines = createInterface({ input, crlfDelay: Infinity })
  try {
    for await (const line of lines) {
      const e = JSON.parse(line)
      if (e.type !== 'slot.started' || e.node !== ownIdentity || e.slot < 48 || e.slot > 95 || starts.has(e.slot)) continue
      const wallMs = Date.parse(e.data.wallTime); assert.ok(Number.isFinite(wallMs))
      starts.set(e.slot, { slot: e.slot, wallMs, wallTime: e.data.wallTime })
    }
  } finally { lines.close(); input.destroy() }
  const missingStarts = Array.from({ length: 48 }, (_, i) => i + 48).filter(slot => !starts.has(slot))
  const result = { scenario, protocol, seed, seedIndex, ...exit, timedOut, outcome,
    slotStarts: [...starts.values()].sort((a, b) => a.slot - b.slot), missingSlotStarts: missingStarts,
    ...slotMetrics(starts.values()), finalityUs: finalityMetrics((outcome.yardstick?.cblocks ?? []).map(row => ({ cblock: row.cblock, status: row.status, finalizedUs: row.finalizedUs }))) }
  if (missingStarts.length) { result.eventWallSlotsPerSecond = null; result.slotPaceAvailable = false }
  if (outcome.status !== 'done' || missingStarts.length || timedOut || exit.exitCode !== 0) {
    const file = join(dirname(reportPath), `r24-handover-${platform}-${scenario}-${protocol}-seed${seedIndex}-${Date.now()}-events.jsonl.gz`)
    await mkdir(dirname(file), { recursive: true })
    await pipeline(createReadStream(join(dir, 'events.jsonl')), createGzip(), createWriteStream(file, { flags: 'wx' }))
    result.diagnosticEvents = relative(backend, file).split(sep).join('/')
  }
  event('handover_lab_finished', { scenario, protocol, seed, status: outcome.status, slotsPerSecond: result.eventWallSlotsPerSecond,
    handoverMedianMs: result.handoverMedianMs, missingSlotStarts: missingStarts, slotsSkipped: outcome.yardstick?.slotsSkipped ?? null })
  await rm(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  return result
}
function comparison(labs) {
  const totals = protocol => {
    const own = labs.filter(row => row.protocol === protocol)
    if (own.length !== 6 || own.some(row => row.missingSlotStarts.length || row.eventWallSlotsPerSecond === null)) return null
    return own.reduce((sum, row) => sum + row.lastSlot - row.firstSlot, 0) * 1000 / own.reduce((sum, row) => sum + row.elapsedMs, 0)
  }
  const agreement = totals('agreement'), lanes = totals('lanes')
  const labRunsSucceeded = labs.length === 12 && labs.every(row => row.exitCode === 0 && row.signal === null && !row.timedOut && row.outcome.status === 'done')
  const pairs = seeds.flatMap(seed => ['split-commitments', 'crash'].map(scenario => {
    const get = protocol => labs.find(row => row.protocol === protocol && row.seed === seed && row.scenario === scenario)?.eventWallSlotsPerSecond ?? null
    const a = get('agreement'), l = get('lanes')
    return { scenario, seed, agreementSlotsPerSecond: a, lanesSlotsPerSecond: l, lanesOverAgreement: a && l ? l / a : null }
  }))
  return { agreementSlotsPerSecond: agreement, lanesSlotsPerSecond: lanes, lanesOverAgreement: agreement && lanes ? lanes / agreement : null,
    criterion: 'All twelve lab children exit normally without timeout and outcome done; across all six paired scenarios/seeds, lanes slots/s >= 0.98 * agreement slots/s.',
    labRunsSucceeded,
    meetsLinuxTarget: process.platform === 'linux' ? labRunsSucceeded && agreement !== null && lanes !== null && lanes >= agreement * 0.98 : null, pairs }
}

try {
  await access(binary); await freePorts()
  report.binarySha256 = createHash('sha256').update(await readFile(binary)).digest('hex')
  runDir = await mkdtemp(join(target, 'r24-handover-pace-')); await mkdir(join(runDir, 'genesis'))
  const baseEnv = { ...process.env }
  for (const name of Object.keys(baseEnv)) if (name.startsWith('ACRUX_')) delete baseEnv[name]
  const netEnv = { ...baseEnv, ACRUX_BIND: '127.0.0.1', ACRUX_IDENTITY_SEED: randomBytes(32).toString('hex'),
    ACRUX_GENESIS_SEED: 'acrux-dev-seed-v1', ACRUX_PLACEMENTS: join(runDir, 'absent-placements.json'),
    ACRUX_BRIDGE_RPC: 'http://127.0.0.1:5044/left,http://127.0.0.1:5044/right', ACRUX_BRIDGE_PROOF_MAX: '100000',
    ACRUX_BRIDGE_PROOF_GRACE_SLOTS: '1728000' }
  const netGenesis = join(runDir, 'genesis/private-net.json')
  const generated = spawnSync(process.execPath, ['scripts/genesis.mjs', netGenesis], { cwd: chainDir, env: netEnv, windowsHide: true, encoding: 'utf8', timeout: 30_000 })
  assert.equal(generated.status, 0, `Generate the local private-identity registry: ${generated.stderr}`)
  const privateGenesis = JSON.parse(await readFile(netGenesis, 'utf8'))
  for (const entry of privateGenesis.programs) entry.path = resolve(chainDir, entry.path)
  await writeFile(netGenesis, JSON.stringify(privateGenesis))
  const labGenesis = join(runDir, 'genesis/lab.json'), lab = JSON.parse(await readFile(join(chainDir, 'genesis/acrux-net.json'), 'utf8'))
  for (const entry of lab.programs) entry.path = resolve(chainDir, entry.path)
  await writeFile(labGenesis, JSON.stringify(lab))
  const labEnv = { ...baseEnv, ACRUX_BIND: '127.0.0.1', ACRUX_IDENTITY_SEED: '20261116', ACRUX_BRIDGE: join(runDir, 'absent-bridge.json'), ACRUX_PLACEMENTS: join(runDir, 'absent-placements.json') }
  const labOwnIdentity = Keypair.fromSeed(createHash('sha256').update('acrux-local-identity-20261116-0').digest()).publicKey.toBase58()
  const program = new PublicKey(JSON.parse(await readFile(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8')).address)
  for (const protocol of ['today', 'lanes']) {
    report.nets.push(await measuredNet(protocol, netGenesis, netEnv, program)); delete report.activeNetInterval; await saveReport()
  }
  for (let i = 0; i < seeds.length; i++) for (const scenario of ['split-commitments', 'crash']) for (const protocol of ['agreement', 'lanes']) {
    report.labs.push(await measuredLab(scenario, protocol, seeds[i], i + 1, labGenesis, labEnv, labOwnIdentity)); await saveReport()
  }
  report.labComparison = comparison(report.labs)
  report.completed = true; report.finishedAt = new Date().toISOString(); await saveReport()
  event('handover_measurement_completed', { report: relative(backend, reportPath).split(sep).join('/'), platform,
    nets: report.nets.map(({ protocol, slotsPerSecond, handoverMedianMs, finalityUs }) => ({ protocol, slotsPerSecond, handoverMedianMs, p50FinalityUs: finalityUs.p50, p90FinalityUs: finalityUs.p90 })), labComparison: report.labComparison })
  if (process.platform === 'linux' && report.labComparison.meetsLinuxTarget !== true) {
    event('handover_linux_target_failed', { criterion: report.labComparison.criterion, available: report.labComparison.lanesOverAgreement !== null,
      agreementSlotsPerSecond: report.labComparison.agreementSlotsPerSecond, lanesSlotsPerSecond: report.labComparison.lanesSlotsPerSecond })
    process.exitCode = 1
  }
} catch (error) {
  report.failure = error.message; report.logTail = tail; await saveReport()
  throw error
} finally {
  await stopOwned()
  if (runDir) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must remain inside node/target.')
    await rm(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
