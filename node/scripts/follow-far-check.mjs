// F7/B1: run the real net and CLI follower through the SDK far-check's 145 ms each-way
// HTTP delay for at least five minutes. Also check web3 getSupply, compare follower/explorer
// page bytes, report retained payload/RSS, and inject one unavailable page to check re-anchoring.
// Only this script's children, proxy and unique target directory are stopped/removed.
// Usage from backend: node node/scripts/follow-far-check.mjs [--seconds 300]
import assert from 'node:assert/strict'
import { spawn, execFile } from 'node:child_process'
import dgram from 'node:dgram'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import http from 'node:http'
import { createRequire } from 'node:module'
import net from 'node:net'
import { dirname, join, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'

const backend = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const nodeDir = join(backend, 'node'), target = join(nodeDir, 'target')
const require = createRequire(join(backend, 'sdk/package.json'))
const { Connection } = require('@solana/web3.js')
const option = process.argv.indexOf('--seconds')
const seconds = option < 0 ? 300 : Number(process.argv[option + 1])
// --faults N: one proxied call in N is stalled past the follower's 10 s timeout or cut off
// mid-body, alternately; the follower must run on.
const faultOption = process.argv.indexOf('--faults')
const faultEvery = faultOption < 0 ? 0 : Number(process.argv[faultOption + 1])
let proxied = 0, faults = 0
assert.ok(Number.isSafeInteger(seconds) && seconds >= 300, '--seconds must be at least 300.')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const genesis = join(backend, 'chain/genesis/acrux-net.json')
const ports = { rpc: 4930, ws: 4931, metrics: 4989, quic: 4950, proxy: 5056 }
const rpcUrl = `http://127.0.0.1:${ports.rpc}`, proxyUrl = `http://127.0.0.1:${ports.proxy}`
const metricsUrl = `http://127.0.0.1:${ports.metrics}/metrics`
const delayMs = 145, rpcMs = 15_000
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const exec = promisify(execFile)
let stopped = false, netProcess, follower, proxy, runDir
let netTail = '', followerTail = '', stdoutLine = '', fatal
let matched = 0, lastMatchedSlot = null, snapshots = 0, reanchors = 0, matchedAtReanchor = 0, gapArmed = false, gapInjected = false
let requestId = 0
const proxyCalls = new Map(), memory = []
const forwarding = new AbortController()
process.once('SIGINT', () => { stopped = true })
process.once('SIGTERM', () => { stopped = true })
const event = (name, data = {}) => console.log(JSON.stringify({ event: name, ...data }))
function alive() {
  if (stopped) throw new Error('Check interrupted.')
  if (fatal) throw fatal
  for (const [name, child, tail] of [['net', netProcess, netTail], ['follower', follower, followerTail]]) {
    if (child && (child.exitCode !== null || child.signalCode !== null)) throw new Error(`${name} exited early (${child.exitCode ?? child.signalCode}): ${tail.slice(-6000)}`)
  }
}
async function until(label, timeoutMs, read) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    alive()
    const value = await read()
    if (value) return value
    await pause(250)
  }
  throw new Error(`${label}: deadline exceeded.`)
}
async function rpc(method, params = {}, url = rpcUrl) {
  const answer = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: ++requestId, method, params }), signal: AbortSignal.timeout(rpcMs) })
  assert.ok(answer.ok, `${method}: HTTP ${answer.status}`)
  const bytes = Buffer.from(await answer.arrayBuffer()), body = JSON.parse(bytes)
  assert.ok(!body.error, `${method}: ${JSON.stringify(body.error)}`)
  return { value: body.result, bytes: bytes.length }
}
async function freeTcp(port) {
  const probe = net.createServer()
  await new Promise((resolve, reject) => { probe.once('error', reject); probe.listen({ host: '0.0.0.0', port, exclusive: true }, resolve) })
  await new Promise((resolve, reject) => probe.close(error => error ? reject(error) : resolve()))
}
async function freeUdp(port) {
  const probe = dgram.createSocket('udp4')
  try { await new Promise((resolve, reject) => { probe.once('error', reject); probe.bind({ address: '0.0.0.0', port, exclusive: true }, resolve) }) }
  finally {
    try { probe.close() } catch (error) { if (error.code !== 'ERR_SOCKET_DGRAM_NOT_RUNNING') throw error }
  }
}
async function rss(pid) {
  if (process.platform === 'win32') {
    const { stdout } = await exec('powershell.exe', ['-NoProfile', '-Command', `(Get-Process -Id ${pid} -ErrorAction Stop).WorkingSet64`], { windowsHide: true, timeout: 5000 })
    return Number(stdout.trim())
  }
  const status = await readFile(`/proc/${pid}/status`, 'utf8')
  const kilobytes = /^VmRSS:\s+(\d+) kB$/m.exec(status)
  assert.ok(kilobytes, 'The net process must expose VmRSS.')
  return Number(kilobytes[1]) * 1024
}
async function sampleMemory(elapsedSeconds) {
  const response = await fetch(metricsUrl, { signal: AbortSignal.timeout(rpcMs) })
  assert.ok(response.ok)
  const text = await response.text()
  const gauge = name => {
    const match = text.match(new RegExp(`^${name} ([0-9.eE+\\-]+)$`, 'm'))
    assert.ok(match, `Missing retention metric ${name}`)
    const value = Number(match[1]); assert.ok(Number.isFinite(value) && value >= 0)
    return value
  }
  const sample = { elapsedSeconds, retainedSlots: gauge('acrux_follower_retained_slots'), retainedBlockBytes: gauge('acrux_follower_retained_block_bytes'), netRssBytes: await rss(netProcess.pid) }
  assert.ok(sample.retainedSlots <= 2400, 'Follower block retention must stay within its two-minute slot window.')
  assert.ok(Number.isSafeInteger(sample.netRssBytes) && sample.netRssBytes > 0, 'Net process RSS must be readable.')
  memory.push(sample)
  return sample
}
async function stopChild(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return
  const exited = new Promise(resolve => child.once('exit', resolve))
  child.kill('SIGTERM')
  await Promise.race([exited, pause(5000)])
  if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await Promise.race([exited, pause(5000)]) }
  assert.ok(child.exitCode !== null || child.signalCode !== null, `Child ${child.pid} did not stop.`)
}
function observeFollower(chunk) {
  const text = chunk.toString()
  followerTail = (followerTail + text).slice(-64_000)
  stdoutLine += text
  let newline
  while ((newline = stdoutLine.indexOf('\n')) >= 0) {
    const line = stdoutLine.slice(0, newline); stdoutLine = stdoutLine.slice(newline + 1)
    if (line.startsWith('re-anchored at cblock ')) { reanchors++; matchedAtReanchor = matched }
    const slot = /^slot (\d+) re-executed\b/.exec(line)
    if (slot) {
      if (!line.includes('matches')) fatal = new Error(`Follower reported a nonmatching slot: ${line}`)
      matched++; lastMatchedSlot = Number(slot[1])
    }
  }
}

try {
  // Check every endpoint before starting anything; no existing listener is ever stopped.
  for (const port of [ports.rpc, ports.ws, ports.metrics, ports.proxy]) await freeTcp(port)
  for (let port = ports.quic; port < ports.quic + 12; port++) await freeUdp(port)
  runDir = await mkdtemp(join(target, 'follow-far-check-'))
  const env = { ...process.env, ACRUX_BIND: '127.0.0.1', ACRUX_BRIDGE: join(runDir, 'no-bridge.json'), ACRUX_PLACEMENTS: join(runDir, 'no-placements.json') }
  for (const name of ['ACRUX_IDENTITY_SEED', 'ACRUX_BRIDGE_RPC', 'ACRUX_LOG_RPC', 'ACRUX_STATE_WRITE_DELAY_MS', 'ACRUX_STATE_WRITE_FAIL_MS']) delete env[name]
  netProcess = spawn(binary, ['net', 'up', '--genesis', genesis, '--store', join(runDir, 'net'), '--gateway-port', String(ports.rpc), '--ws-port', String(ports.ws), '--metrics-port', String(ports.metrics), '--base-port', String(ports.quic), '--traffic-rate', '50'], { cwd: nodeDir, env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true })
  netProcess.once('error', error => { fatal = error })
  for (const stream of [netProcess.stdout, netProcess.stderr]) stream.on('data', chunk => { netTail = (netTail + chunk).slice(-64_000) })
  await until('Local finalized head', 60_000, async () => {
    try { return (await rpc('getFinalizedCblock')).value.consensusSlot !== null }
    catch (error) { if (error.cause?.code === 'ECONNREFUSED') return false; throw error }
  })

  // Exercise web3's actual getSupply decoder against the gateway's Solana-compatible route.
  const connection = new Connection(rpcUrl, { commitment: 'finalized', disableRetryOnRateLimit: true, fetch: (input, init) => fetch(input, { ...init, signal: AbortSignal.timeout(rpcMs) }) })
  const supply = await connection.getSupply({ commitment: 'finalized', excludeNonCirculatingAccountsList: false })
  assert.ok(Number.isSafeInteger(supply.context.slot) && supply.context.slot >= 0)
  assert.ok(Number.isFinite(supply.value.total) && Number.isInteger(supply.value.total) && supply.value.total > 0)
  assert.equal(supply.value.total, supply.value.circulating)
  assert.equal(supply.value.nonCirculating, 0)
  assert.deepEqual(supply.value.nonCirculatingAccounts, [])
  event('follow_supply_checked', supply)

  proxy = http.createServer((request, response) => {
    let body = ''
    request.on('data', chunk => { body += chunk })
    request.on('end', async () => {
      try {
        const call = JSON.parse(body)
        if (faultEvery && ++proxied % faultEvery === 0) {
          faults++
          if (faults % 2) { await pause(12_000); response.destroy() }
          else { response.writeHead(200, { 'content-type': 'application/json' }); response.write('{"jsonrpc":"2.0","id":1,"result":{"sl'); response.destroy() }
          return
        }
        if (call.method === 'getExecutionSnapshot') snapshots++
        await pause(delayMs)
        const upstream = await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body, signal: AbortSignal.any([forwarding.signal, AbortSignal.timeout(rpcMs)]) })
        let bytes = Buffer.from(await upstream.arrayBuffer())
        let slots = 0
        if (call.method === 'getFollowerPage') {
          const page = JSON.parse(bytes)
          slots = page.result?.slots?.length ?? 0
          // An empty page is the real unavailable-history condition, without modifying signed
          // blocks or state hashes. A fresh snapshot must recover from this availability gap.
          if (gapArmed && !gapInjected && slots > 0) {
            page.result.slots = []; bytes = Buffer.from(JSON.stringify(page)); slots = 0; gapInjected = true
            event('follow_gap_injected', { from: call.params.from })
          }
        }
        const total = proxyCalls.get(call.method) ?? { calls: 0, bytes: 0, slots: 0 }
        total.calls++; total.bytes += bytes.length; total.slots += slots
        proxyCalls.set(call.method, total)
        await pause(delayMs)
        response.writeHead(upstream.status, { 'content-type': 'application/json' }).end(bytes)
      } catch (error) {
        if (!forwarding.signal.aborted) fatal = error
        if (!response.destroyed) response.writeHead(502).end()
      }
    })
  })
  proxy.keepAliveTimeout = 65_000; proxy.headersTimeout = 70_000
  await new Promise((resolve, reject) => { proxy.once('error', reject); proxy.listen(ports.proxy, '127.0.0.1', resolve) })
  follower = spawn(binary, ['node', 'follow', proxyUrl, '--genesis', genesis], { cwd: nodeDir, env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true })
  follower.once('error', error => { fatal = error })
  follower.stdout.on('data', observeFollower)
  follower.stderr.on('data', chunk => { followerTail = (followerTail + chunk).slice(-64_000) })
  await until('Follower matching real state hashes', 60_000, async () => matched >= 20)
  const started = Date.now(), firstMatched = matched, firstSnapshotCount = snapshots
  event('follow_far_started', { netPid: netProcess.pid, followerPid: follower.pid, ports, delayEachWayMs: delayMs, durationSeconds: seconds })
  let nextProgress = 0, previousMatched = matched
  const lag = []
  while (Date.now() - started < seconds * 1000) {
    alive()
    const elapsed = (Date.now() - started) / 1000
    if (elapsed >= nextProgress) {
      const head = (await rpc('getFinalizedCblock')).value.consensusSlot
      const behindSlots = head * 4 + 3 - lastMatchedSlot
      lag.push({ elapsedSeconds: Math.round(elapsed), behindSlots })
      const sample = await sampleMemory(Math.round(elapsed))
      if (nextProgress > 0) assert.ok(matched > previousMatched, 'Follower stopped making progress.')
      assert.ok(behindSlots < 2400, 'Far follower fell beyond the retained block window.')
      previousMatched = matched
      event('follow_far_progress', { elapsedSeconds: Math.round(elapsed), matched: matched - firstMatched, lastMatchedSlot, behindSlots, snapshots, ...sample })
      nextProgress += 30
    }
    await pause(1000)
  }
  alive()
  if (faultEvery) { assert.ok(faults > 0, 'No faults were injected.'); event('follow_faults_survived', { faults, every: faultEvery }) }
  else assert.equal(snapshots, firstSnapshotCount, 'The healthy five-minute run must keep up without snapshot resets.')
  assert.ok(matched > firstMatched)
  const head = (await rpc('getFinalizedCblock')).value.consensusSlot
  const finalBehindSlots = head * 4 + 3 - lastMatchedSlot
  assert.ok(finalBehindSlots < 128, 'The healthy far follower must finish within 128 slots (6.4 seconds) of the live chain.')
  // A range that starts on a skipped slot answers empty; step back to one the gateway holds.
  let from, legacy, compact
  for (let back = 15; back < 60; back++) {
    from = Math.max(0, (head - back) * 4)
    legacy = await rpc('getSlotRange', { from, count: 64 })
    compact = await rpc('getFollowerPage', { from, count: 64 })
    if (legacy.value.length > 0 && compact.value.slots.length > 0) break
  }
  assert.ok(legacy.value.length > 0 && compact.value.slots.length > 0)
  const wire = { from, legacySlots: legacy.value.length, followerSlots: compact.value.slots.length, legacyBytesPerSlot: legacy.bytes / legacy.value.length, followerBytesPerSlot: compact.bytes / compact.value.slots.length }
  assert.ok(wire.followerBytesPerSlot < wire.legacyBytesPerSlot, 'Follower pages must remove explorer overhead.')
  const finalMemory = await sampleMemory(seconds)
  event('follow_payload_measured', { ...wire, ...finalMemory })
  assert.ok(head * 4 >= 2404, 'The net must advance beyond the two-minute block window.')
  const expired = (await rpc('getFollowerPage', { from: 0, count: 4 })).value
  assert.equal(expired.slots.length, 4)
  assert.ok(expired.slots.every(slot => slot.blocks === null), 'Expired follower history must return unavailable blocks without an RPC error.')
  event('follow_expired_history_checked', { from: 0, slots: expired.slots.length })

  const beforeReanchor = reanchors, beforeSnapshot = snapshots
  gapArmed = true
  await until('A deliberate unavailable page', 30_000, async () => gapInjected)
  await until('Follower snapshot re-anchor and matching progress', 60_000, async () => snapshots > beforeSnapshot && reanchors > beforeReanchor && matched >= matchedAtReanchor + 40)
  alive()
  event('follow_far_passed', { durationSeconds: (Date.now() - started) / 1000, matchedSlots: matched - firstMatched, lastMatchedSlot, snapshots, reanchors, finalBehindSlots, gapRecovered: true, wire, lag, memory, rpc: Object.fromEntries(proxyCalls) })
} finally {
  forwarding.abort()
  await stopChild(follower)
  if (proxy?.listening) {
    proxy.closeAllConnections()
    await new Promise((resolve, reject) => proxy.close(error => error ? reject(error) : resolve()))
  }
  await stopChild(netProcess)
  if (runDir) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must stay within node/target.')
    await rm(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
}
