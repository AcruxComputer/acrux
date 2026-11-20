// Build 6 steps 1/2: six paired Fault Lab runs, once each, on the same seed.
// Usage from backend, after the coordinated release build:
// node node/scripts/lab-agreement-check.mjs
// After a genuine agreement fix: add --agreement-only to reuse the recorded today baselines.
// Only twelve loopback QUIC ports 5062..5073 are used; lab opens no HTTP listeners.
// Raw runs stay under node/target and are removed after reporting; failed event streams are retained as gzip diagnostics.
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import dgram from 'node:dgram'
import { createReadStream, createWriteStream } from 'node:fs'
import { access, mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { pipeline } from 'node:stream/promises'
import { fileURLToPath } from 'node:url'
import { createGzip } from 'node:zlib'

const backend = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const nodeDir = join(backend, 'node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const sourceGenesis = join(backend, 'chain/genesis/acrux-net.json')
const reportPath = join(nodeDir, 'reports/r20-agreement.json')
const baselinePath = join(nodeDir, 'reports/r20-agreement-initial.json')
const agreementOnly = process.argv.slice(2).length === 1 && process.argv[2] === '--agreement-only'
assert.ok(process.argv.length === 2 || agreementOnly, 'Only --agreement-only is supported; the seed and scenarios stay fixed.')
const seed = 'acrux-r20-agreement', basePort = 5062, attackerWindow = 4
const cases = ['split-commitments', 'crash', 'censorship'].flatMap(scenario => ['today', 'agreement'].map(protocol => ({ scenario, protocol })))
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
  }
  const attack = yardstick.cblocks.filter(row => row.window === attackerWindow)
  require(yardstick.slotsSkipped === attack.filter(row => row.skipped).length * 4, 'Attacker-window skipped slots must match its cblock rows.')
  const evidence = outcome.evidence ?? []
  if (item.scenario === 'split-commitments' && item.protocol === 'today') {
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
function windowSummary(outcome) {
  const rows = outcome?.yardstick?.cblocks ?? []
  return [3, 4, 5].map(window => {
    const own = rows.filter(row => row.window === window)
    return { window, firstSlot: window * 16, lastSlot: window * 16 + 15,
      finalized: own.filter(row => row.status === 'finalized').length,
      skipped: own.filter(row => row.status === 'skipped').length,
      pending: own.filter(row => row.status === 'pending').length,
      slotsSkipped: own.filter(row => row.skipped).length * 4,
      statuses: own.map(row => ({ cblock: row.cblock, status: row.status })) }
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
  const output = join(dirname(reportPath), `r20-${item.scenario}-${item.protocol}-${started}-events.jsonl.gz`)
  await mkdir(dirname(output), { recursive: true })
  await pipeline(createReadStream(input), createGzip(), createWriteStream(output, { flags: 'wx' }))
  return { path: relative(backend, output).split(sep).join('/'), compression: 'gzip',
    uncompressedBytes: raw.size, compressedBytes: (await stat(output)).size }
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
  item.windows = windowSummary(outcome)
  item.nonAttackSlotsSkipped = item.windows.filter(row => row.window !== attackerWindow).reduce((sum, row) => sum + row.slotsSkipped, 0)
  item.problems = validateOutcome(item)
  if (item.problems.length) {
    item.logTail = tail
    item.diagnosticEvents = await preserveFailedEvents(dir, item, started)
  }
  event('lab_run_finished', { scenario, protocol, status: outcome?.status ?? 'no_outcome',
    slotsSkipped: outcome?.yardstick?.slotsSkipped ?? null, nonAttackSlotsSkipped: item.nonAttackSlotsSkipped, problems: item.problems })
  return item
}

try {
  await access(binary)
  await freePorts()
  root = await mkdtemp(join(target, 'r20-agreement-'))
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
  const byCase = new Map()
  if (agreementOnly) {
    const baseline = JSON.parse(await readFile(baselinePath, 'utf8'))
    assert.equal(baseline.seed, seed, 'Corrective verification keeps the exact original seed.')
    assert.equal(baseline.basePort, basePort)
    assert.equal(baseline.trafficPerSecond, 50)
    assert.equal(baseline.runSeconds, 12)
    assert.match(baseline.binarySha256, /^[0-9a-f]{64}$/)
    for (const requested of cases.filter(item => item.protocol === 'today')) {
      const found = baseline.runs.filter(item => item.scenario === requested.scenario && item.protocol === 'today')
      assert.equal(found.length, 1, 'Each original today baseline must appear exactly once.')
      const item = structuredClone(found[0])
      item.binarySha256 = baseline.binarySha256
      item.reusedFrom = relative(backend, baselinePath).split(sep).join('/')
      // Evaluate the preserved evidence with the corrected scenario requirements; never alter
      // the original report or hide its initial agreement failures.
      item.problems = validateOutcome(item)
      byCase.set(`${item.scenario}/${item.protocol}`, item)
    }
  }
  for (const requested of cases.filter(item => !agreementOnly || item.protocol === 'agreement')) {
    const item = await runOnce(requested, genesisPath, env, binarySha256)
    byCase.set(`${item.scenario}/${item.protocol}`, item)
  }
  const runs = cases.map(item => byCase.get(`${item.scenario}/${item.protocol}`))
  assert.ok(runs.every(Boolean), 'The final comparison must retain all six scenario/protocol rows.')
  const report = { check: 'r20-lab-agreement', startedAt, finishedAt: new Date().toISOString(),
    version, binary: relative(backend, binary).split(sep).join('/'), binarySha256,
    correctiveVerification: agreementOnly,
    ...(agreementOnly ? { baselineReport: relative(backend, baselinePath).split(sep).join('/'), executedCases: 3, reusedCases: 3 } : { executedCases: 6, reusedCases: 0 }),
    seed, basePort, trafficPerSecond: 50, runSeconds: 12, attackerWindow,
    comparedWindows: [3, 4, 5], passed: runs.every(item => item.problems.length === 0), runs }
  await mkdir(dirname(reportPath), { recursive: true })
  await writeFile(reportPath, JSON.stringify(report, null, 2) + '\n')
  event('lab_agreement_report', { report: relative(backend, reportPath).split(sep).join('/'), runs: runs.length, passed: report.passed })
  assert.ok(report.passed, `Lab comparison failed; inspect ${relative(backend, reportPath)}. Each selected case ran once; recorded today baselines were reused only with --agreement-only.`)
} finally {
  await stopOwned()
  if (root) {
    assert.ok(resolve(root).startsWith(resolve(target) + sep), 'Cleanup must stay inside node/target.')
    await rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
