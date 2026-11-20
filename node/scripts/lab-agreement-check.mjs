// R24 / early lanes window handover: nine fresh Fault Lab runs and the silent-lane finalization timing check.
// Usage from backend, after the coordinated release build:
// node node/scripts/lab-agreement-check.mjs
// Only twelve loopback QUIC ports 5062..5073 are used; lab opens no HTTP listeners.
// Raw runs stay under node/target and are removed after reporting; failed event streams are retained as gzip diagnostics.
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import dgram from 'node:dgram'
import { createReadStream, createWriteStream } from 'node:fs'
import { access, mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { createInterface } from 'node:readline'
import { pipeline } from 'node:stream/promises'
import { fileURLToPath } from 'node:url'
import { createGzip } from 'node:zlib'

const backend = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const nodeDir = join(backend, 'node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const sourceGenesis = join(backend, 'chain/genesis/acrux-net.json')
const reportPath = join(nodeDir, 'reports/r24-handover.json')
assert.equal(process.argv.length, 2, 'R24 runs all nine fresh cases; the seed and scenarios stay fixed.')
const seed = 'acrux-r20-agreement', basePort = 5062, attackerWindow = 4
const cases = ['split-commitments', 'crash', 'censorship'].flatMap(scenario => ['today', 'agreement', 'lanes'].map(protocol => ({ scenario, protocol })))
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
let root, current, interrupted = false
const event = (name, fields) => console.log(JSON.stringify({ event: name, ...fields }))
function interrupt() { interrupted = true; current?.kill('SIGTERM') }
process.once('SIGINT', interrupt)
process.once('SIGTERM', interrupt)

async function freePorts() {
  for (let port = basePort; port < basePort + 12; port++) {
    const socket = dgram.createSocket('udp4')
    try { await new Promise((resolve, reject) => { socket.once('error', reject); socket.bind({ port, address: '127.0.0.1', exclusive: true }, resolve) }) }
    finally { try { socket.close() } catch (error) { if (error.code !== 'ERR_SOCKET_DGRAM_NOT_RUNNING') throw error } }
  }
}
async function stopOwned() {
  if (!current?.pid || current.exitCode !== null || current.signalCode !== null) return
  const child = current, closed = new Promise(resolve => child.once('close', resolve))
  child.kill('SIGTERM'); await Promise.race([closed, pause(5000)])
  if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await Promise.race([closed, pause(5000)]) }
  assert.ok(child.exitCode !== null || child.signalCode !== null, 'The check-owned lab process did not stop.')
}
function validateOutcome(item) {
  const problems = []
  const require = (ok, message) => { if (!ok) problems.push(message) }
  require(item.exitCode === 0 && item.signal === null && !item.timedOut, 'Lab process must exit normally within its deadline.')
  const outcome = item.outcome
  if (!outcome) return [...problems, 'No outcome.json was produced.']
  require(outcome.protocol === item.protocol, 'The outcome must name the requested lab protocol.')
  require(outcome.status === 'done' && outcome.failure === null, 'The lab must gather its required evidence.')
  require(Array.isArray(outcome.evidence), 'The lab must return evidence.')
  const yardstick = outcome.yardstick
  if (!yardstick || !Array.isArray(yardstick.cblocks)) return [...problems, 'The per-cblock yardstick is missing.']
  require(yardstick.attackerWindow === attackerWindow, 'The attacker must remain in window 4.')
  require(yardstick.cblocks.length === 12, 'The yardstick must preserve all twelve cblocks across windows 3, 4 and 5.')
  require(yardstick.cblocks.every((row, index) => row.cblock === 12 + index && row.window === Math.floor(row.cblock / 4)), 'Cblock rows must be contiguous and belong to their recorded window.')
  for (const row of yardstick.cblocks) {
    require(['pending', 'finalized', 'skipped'].includes(row.status), `Invalid status for cblock ${row.cblock}.`)
    require(row.skipped === (row.status === 'skipped'), `Skip status differs for cblock ${row.cblock}.`)
    require(row.votes && Object.values(row.votes).every(value => Number.isSafeInteger(value) && value >= 0), `Invalid vote counts for cblock ${row.cblock}.`)
    require(Array.isArray(row.certificates), `Missing certificate list for cblock ${row.cblock}.`)
    require(row.finalizedUs === null || Number.isSafeInteger(row.finalizedUs) && row.finalizedUs >= 0, `Invalid measured finalization time for cblock ${row.cblock}.`)
    require(row.status !== 'finalized' || row.finalizedUs !== null, `Finalized cblock ${row.cblock} needs its observed finalization time.`)
    if (item.protocol === 'lanes') {
      const lanes = row.lanes
      require(Array.isArray(lanes) && lanes.length === 3 && [1, 2, 3].every(proposer => lanes.filter(lane => lane.proposer === proposer).length === 1),
        `Cblock ${row.cblock} must retain all three independent lanes.`)
      for (const lane of lanes ?? []) {
        require(['pending', 'finalized', 'skipped'].includes(lane.status), `Invalid lane ${lane.proposer} status at cblock ${row.cblock}.`)
        require(lane.votes && Object.values(lane.votes).every(value => Number.isSafeInteger(value) && value >= 0), `Invalid lane vote counts at cblock ${row.cblock}.`)
        require(Array.isArray(lane.certificates), `Missing lane certificates at cblock ${row.cblock}.`)
      }
    }
  }
  const attack = yardstick.cblocks.filter(row => row.window === attackerWindow)
  require(yardstick.slotsSkipped === attack.filter(row => row.skipped).length * 4, 'Attacker-window skipped slots must match its cblock rows.')
  const evidence = outcome.evidence ?? []
  if (item.protocol === 'lanes' && ['split-commitments', 'crash'].includes(item.scenario)) {
    require(yardstick.slotsSkipped === 0 && attack.every(row => row.status === 'finalized'),
      'Independent honest lanes must keep all four attacker-window cblocks finalized.')
    for (const row of attack) {
      require([1, 2].every(proposer => row.lanes?.some(lane => lane.proposer === proposer && lane.status === 'finalized')),
        `Honest lanes 1 and 2 must finalize at cblock ${row.cblock}.`)
      require(row.lanes?.some(lane => lane.proposer === 3 && lane.status === 'skipped'),
        `Only attacker lane 3 must skip at cblock ${row.cblock}.`)
    }
    require(attack.some(row => row.lanes?.some(lane => lane.proposer === 3 && lane.certificates.includes('skip'))),
      'The skipped attacker lane must have its actual skip certificate.')
    if (item.scenario === 'split-commitments') require(evidence.some(row => row.kind === 'lane_skipped' && row.proposer === 3
      && row.skipCertificate?.kind === 'skip' && row.skipCertificate.hash === null
      && row.skipCertificate.signers?.length > 0 && row.signatures?.length > 0
      && JSON.stringify(row.otherLanesFinalized) === '[1,2]'),
      'Lane evidence must preserve the skip certificate, signatures and finalized honest lanes.')
    if (item.scenario === 'crash') require(evidence.some(row => row.kind === 'two_proposers_continue'), 'Crash still needs the existing two-proposer evidence.')
    if (item.scenario === 'crash') {
      const timing = item.crashTiming
      require(timing?.status === 'measured', 'Crash timing needs all four finalized measurements in both the previous and attacker windows.')
      if (timing?.status === 'measured') for (const row of timing.comparisons) {
        require(row.withinLimit, `Crash cblock ${row.cblock} finalized in ${row.finalizedUs} us, above the previous-window median ${timing.baselineMedianUs} us plus 100000 us.`)
      }
    }
  } else if (item.scenario === 'split-commitments' && item.protocol === 'today') {
    require(yardstick.slotsSkipped === 16, 'Today must expose the whole skipped attacker window.')
    require(evidence.some(row => row.kind === 'skip_certificate'), 'Today split commitments need their skip certificate evidence.')
  } else if (item.scenario === 'split-commitments') {
    require(yardstick.slotsSkipped === 0, 'The split attacker window must continue without skipped slots.')
    require(attack.every(row => row.status === 'finalized'), 'All four cblocks of the continuing split window must finalize.')
    require(evidence.some(row => row.kind === 'window_continued' && row.finalized === true),
      'Agreement continuation must be finalized, with the original attack evidence retained.')
  } else {
    // These checks retain their existing evidence requirement. Ordinary skipped cblocks
    // stay visible in the yardstick instead of becoming a new acceptance criterion.
    const kind = item.scenario === 'crash' ? 'two_proposers_continue' : 'transaction_included'
    require(evidence.some(row => row.kind === kind), `Missing ${kind} evidence.`)
  }
  return problems
}
function crashTiming(outcome) {
  const rows = outcome?.yardstick?.cblocks ?? []
  const windows = [3, 4, 5].map(window => {
    const cblocks = Array.from({ length: 4 }, (_, index) => {
      const cblock = window * 4 + index, row = rows.find(row => row.cblock === cblock && row.window === window)
      return { cblock, status: row?.status ?? 'missing', finalizedUs: row?.finalizedUs ?? null }
    })
    const valid = row => row.status === 'finalized' && Number.isSafeInteger(row.finalizedUs) && row.finalizedUs >= 0
    const missingCblocks = cblocks.filter(row => !valid(row)).map(row => row.cblock)
    const ordered = cblocks.filter(valid).map(row => row.finalizedUs).sort((a, b) => a - b)
    // All four baseline observations are required. A skipped/pending/missing measurement
    // cannot silently lower the median or become a fabricated zero.
    const medianUs = missingCblocks.length ? null : (ordered[1] + ordered[2]) / 2
    return { window, cblocks, missingCblocks, medianUs }
  })
  const baselineMedianUs = windows[0].medianUs
  const allowanceUs = 100_000
  const limitUs = baselineMedianUs === null ? null : baselineMedianUs + allowanceUs
  const complete = windows[0].missingCblocks.length === 0 && windows[1].missingCblocks.length === 0
  const comparisons = windows[1].cblocks.map(row => ({ ...row,
    aboveBaselineUs: baselineMedianUs === null || windows[1].missingCblocks.includes(row.cblock) ? null : row.finalizedUs - baselineMedianUs,
    withinLimit: limitUs === null || windows[1].missingCblocks.includes(row.cblock) ? null : row.finalizedUs <= limitUs,
  }))
  return { baselineWindow: 3, attackWindow: 4, allowanceUs, medianMethod: 'Arithmetic mean of the middle two of four finalized cblocks',
    windows, baselineMedianUs, limitUs, status: complete ? 'measured' : 'unavailable',
    passed: complete ? comparisons.every(row => row.withinLimit) : null, comparisons }
}
function windowSummary(outcome) {
  const rows = outcome?.yardstick?.cblocks ?? []
  return [3, 4, 5].map(window => {
    const own = rows.filter(row => row.window === window)
    return { window, firstSlot: window * 16, lastSlot: window * 16 + 15,
      finalized: own.filter(row => row.status === 'finalized').length,
      skipped: own.filter(row => row.status === 'skipped').length,
      pending: own.filter(row => row.status === 'pending').length,
      slotsSkipped: own.filter(row => row.skipped).length * 4,
      statuses: own.map(row => ({ cblock: row.cblock, status: row.status, finalizedUs: row.finalizedUs })),
      lanes: [1, 2, 3].flatMap(proposer => {
        const laneRows = own.flatMap(row => (row.lanes ?? []).filter(lane => lane.proposer === proposer).map(lane => ({ cblock: row.cblock, status: lane.status })))
        return laneRows.length ? [{ proposer, finalized: laneRows.filter(row => row.status === 'finalized').length,
          skipped: laneRows.filter(row => row.status === 'skipped').length, pending: laneRows.filter(row => row.status === 'pending').length,
          statuses: laneRows }] : []
      }) }
  })
}
async function preserveFailedEvents(dir, item, started) {
  const input = join(dir, 'events.jsonl')
  let raw
  try { raw = await stat(input) }
  catch (error) {
    if (error.code !== 'ENOENT') throw error
    return { unavailable: 'The failed lab process produced no events.jsonl.' }
  }
  const output = join(dirname(reportPath), `r24-${item.scenario}-${item.protocol}-${started}-events.jsonl.gz`)
  await mkdir(dirname(output), { recursive: true })
  await pipeline(createReadStream(input), createGzip(), createWriteStream(output, { flags: 'wx' }))
  return { path: relative(backend, output).split(sep).join('/'), compression: 'gzip',
    uncompressedBytes: raw.size, compressedBytes: (await stat(output)).size }
}
async function readEventContext(dir, crash) {
  const windows = new Map(), firstReturnedSlot = new Map()
  let truncated = false
  const input = createReadStream(join(dir, 'events.jsonl'), { encoding: 'utf8' })
  const lines = createInterface({ input, crlfDelay: Infinity })
  const relevant = new Set(['commitment.sent', 'commitment.arrived', 'attestation.sent', 'attestation.arrived', 'slot.agreed', 'slot.executed', 'slot.skipped'])
  try {
    for await (const line of lines) {
      if (!line.trim()) continue
      const event = JSON.parse(line), data = event.data
      if (event.type === 'window.started' && [3, 4, 5].includes(data.number) && !windows.has(data.number)) {
        windows.set(data.number, { window: data.number, hubCode: data.hubCode,
          proposers: data.proposers.map(({ index, identity }) => ({ proposer: index, identity })) })
      }
      if (!crash || event.slot !== 80 || !relevant.has(event.type)) continue
      const proposer = data.proposer ?? data.attestation?.proposer ?? null
      const key = `${event.type}/${event.node ?? ''}/${proposer}/${data.arrival?.receiver ?? ''}/${data.validator ?? ''}`
      if (firstReturnedSlot.has(key)) continue
      if (firstReturnedSlot.size >= 128) { truncated = true; continue }
      firstReturnedSlot.set(key, { type: event.type, node: event.node ?? null, atUs: event.atUs,
        proposer, hash: data.hash ?? null, pi: data.pi ?? data.attestation?.pi ?? null,
        receiver: data.arrival?.receiver ?? null, relativeToStartUs: data.arrival?.relativeToStartUs ?? null,
        validator: data.validator ?? null, agreedSet: data.agreedSet ?? null,
        stateHash: data.stateHash ?? null, reason: data.reason ?? null })
    }
  } catch (error) { if (error.code !== 'ENOENT') throw error }
  finally { lines.close(); input.destroy() }
  const proposerWindows = [...windows.values()].sort((a, b) => a.window - b.window)
  const crashedIdentity = windows.get(4)?.proposers.find(row => row.proposer === 3)?.identity ?? null
  const nextRole = crashedIdentity ? windows.get(5)?.proposers.find(row => row.identity === crashedIdentity)?.proposer ?? null : null
  return { proposerWindows, ...(crash ? { postCrash: { firstSlot: 80, crashedIdentity, nextWindowRole: nextRole,
    firstSlotEvents: [...firstReturnedSlot.values()], traceTruncated: truncated } } : {}) }
}
async function runOnce({ scenario, protocol }, genesis, env, binarySha256) {
  if (interrupted) throw new Error('Lab comparison interrupted.')
  await freePorts()
  const dir = join(root, `${scenario}-${protocol}`)
  await mkdir(dir)
  const args = ['lab', 'run', '--scenario', scenario, '--protocol', protocol, '--seed', seed,
    '--base-port', String(basePort), '--out', dir, '--genesis', genesis]
  let tail = '', timedOut = false, spawnError
  const started = Date.now()
  event('lab_run_started', { scenario, protocol, seed })
  current = spawn(binary, args, { cwd: dir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
  for (const stream of [current.stdout, current.stderr]) stream.on('data', bytes => { tail = (tail + bytes).slice(-16000) })
  const child = current
  const timer = setTimeout(() => { timedOut = true; child.kill('SIGTERM') }, 45000)
  let hardTimer
  const exited = new Promise(resolve => {
    child.once('error', error => { spawnError = error.message })
    child.once('close', (exitCode, signal) => resolve({ exitCode, signal }))
    hardTimer = setTimeout(() => { if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL') }, 50000)
  })
  const result = await exited
  clearTimeout(timer); clearTimeout(hardTimer)
  current = undefined
  let outcome = null
  try { outcome = JSON.parse(await readFile(join(dir, 'outcome.json'), 'utf8')) }
  catch (error) { if (error.code !== 'ENOENT') throw error }
  const item = { scenario, protocol, binarySha256, durationMs: Date.now() - started, ...result, timedOut,
    ...(spawnError ? { spawnError } : {}), outcome }
  Object.assign(item, await readEventContext(dir, scenario === 'crash'))
  item.windows = windowSummary(outcome)
  if (item.postCrash) item.postCrash.window = item.windows.find(row => row.window === 5)
  item.nonAttackSlotsSkipped = item.windows.filter(row => row.window !== attackerWindow).reduce((sum, row) => sum + row.slotsSkipped, 0)
  if (scenario === 'crash') item.crashTiming = crashTiming(outcome)
  item.problems = validateOutcome(item)
  if (item.problems.length) {
    item.logTail = tail
    item.diagnosticEvents = await preserveFailedEvents(dir, item, started)
  }
  event('lab_run_finished', { scenario, protocol, status: outcome?.status ?? 'no_outcome',
    slotsSkipped: outcome?.yardstick?.slotsSkipped ?? null, nonAttackSlotsSkipped: item.nonAttackSlotsSkipped,
    ...(item.crashTiming ? { crashTiming: { status: item.crashTiming.status, previousMedianUs: item.crashTiming.baselineMedianUs,
      limitUs: item.crashTiming.limitUs, attackerFinalizedUs: item.crashTiming.comparisons.map(row => row.finalizedUs),
      withinLimit: item.crashTiming.passed, required: protocol === 'lanes' } } : {}), problems: item.problems })
  return item
}

try {
  await access(binary)
  await freePorts()
  root = await mkdtemp(join(target, 'r24-handover-'))
  const genesis = JSON.parse(await readFile(sourceGenesis, 'utf8'))
  for (const program of genesis.programs) program.path = resolve(backend, 'chain', program.path)
  const genesisPath = join(root, 'genesis.json')
  await writeFile(genesisPath, JSON.stringify(genesis))
  const env = { ...process.env }
  // The Lab keeps its source defaults; a live host's private bridge/fee/profile settings do not
  // configure this fresh unbridged sandbox, and its unique cwd contains no local .env.
  for (const name of Object.keys(env)) if (name.startsWith('ACRUX_')) delete env[name]
  Object.assign(env, { ACRUX_BIND: '127.0.0.1', ACRUX_IDENTITY_SEED: '20261116',
    ACRUX_BRIDGE: join(root, 'absent-bridge.json'), ACRUX_PLACEMENTS: join(root, 'absent-placements.json') })
  const version = /^version = "([^"]+)"/m.exec(await readFile(join(nodeDir, 'crates/acrux-node/Cargo.toml'), 'utf8'))?.[1]
  const binarySha256 = createHash('sha256').update(await readFile(binary)).digest('hex')
  const startedAt = new Date().toISOString()
  const runs = []
  for (const requested of cases) runs.push(await runOnce(requested, genesisPath, env, binarySha256))
  const report = { check: 'r24-handover', startedAt, finishedAt: new Date().toISOString(),
    version, binary: relative(backend, binary).split(sep).join('/'), binarySha256,
    executedCases: 9, reusedCases: 0, seed, basePort, trafficPerSecond: 50, runSeconds: 12, attackerWindow,
    crashTimingRequirement: { protocol: 'lanes', comparison: 'Each window4 cblock finalizedUs <= median of all four finalized window3 cblocks + 100000 us', missingMeasurements: 'Unavailable and failed; no zero or partial baseline' },
    comparedWindows: [3, 4, 5], passed: runs.every(item => item.problems.length === 0), runs }
  await mkdir(dirname(reportPath), { recursive: true })
  await writeFile(reportPath, JSON.stringify(report, null, 2) + '\n')
  event('lab_lanes_net_report', { report: relative(backend, reportPath).split(sep).join('/'), runs: runs.length, passed: report.passed })
  assert.ok(report.passed, `Lab comparison failed; inspect ${relative(backend, reportPath)}. All nine cases ran once from fresh stores; no historical baseline was reused.`)
} finally {
  await stopOwned()
  if (root) {
    assert.ok(resolve(root).startsWith(resolve(target) + sep), 'Cleanup must stay inside node/target.')
    await rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
