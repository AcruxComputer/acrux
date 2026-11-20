// Build the node and examples with node/scripts/build.ps1, then run from backend:
// node node/scripts/propose-observer-check.mjs [--far]
// --far puts the observer's RPC behind a proxy that holds each call 145 ms each way and streams
// answers at 1 MB/s, as from a desktop to the public host, so its snapshot is seconds old when it links.
// It also points getHubPeers at a UDP relay that holds every datagram 145 ms each way, so the QUIC
// link and its replay run at a far observer's round trip.
// Owns only these children and a unique node/target directory. No Solana validator is needed.
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import dgram from 'node:dgram'
import { access, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import http from 'node:http'
import net from 'node:net'
import { dirname, join, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const backend = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const nodeDir = join(backend, 'node'), target = join(nodeDir, 'target')
const require = createRequire(join(backend, 'sdk/package.json'))
const { Keypair } = require('@solana/web3.js')
const executable = name => join(target, 'release', name + (process.platform === 'win32' ? '.exe' : ''))
const binary = executable('acrux')
const probeBinary = join(target, 'release', 'examples', 'propose-peer-probe' + (process.platform === 'win32' ? '.exe' : ''))
const ports = { rpc: 5041, ws: 5042, metrics: 5043, quic: 5062, external: 5074 }
const rpcUrl = `http://127.0.0.1:${ports.rpc}`
const far = process.argv.includes('--far'), farPort = 5059, farUrl = `http://127.0.0.1:${farPort}`, relayPort = 5075
const event = (event, fields = {}) => console.log(JSON.stringify({ event, ...fields }))
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const children = new Set(), roundTrips = [], matchedSlots = new Set()
let runDir, env, fatal, stopped = false, netChild, observer, observerLine = '', netTail = '', observerTail = ''
let lastMatchedSlot = null, authenticatedSlowPeer = false, droppedSlowPeer = false, linkLosses = 0
let requestId = 0, farProxy, farRelay
process.once('SIGINT', () => { stopped = true })
process.once('SIGTERM', () => { stopped = true })

function alive() {
  if (stopped) throw new Error('Check interrupted.')
  if (fatal) throw fatal
  for (const [name, child, tail] of [['net', netChild, netTail], ['observer', observer, observerTail]]) {
    if (child && (child.exitCode !== null || child.signalCode !== null)) {
      throw new Error(`${name} exited early (${child.exitCode ?? child.signalCode}): ${tail.slice(-6000)}`)
    }
  }
}
async function until(label, milliseconds, read) {
  const deadline = Date.now() + milliseconds
  while (Date.now() < deadline) {
    alive()
    const value = await read()
    if (value) return value
    await pause(100)
  }
  throw new Error(`${label}: deadline exceeded.`)
}
async function rpc(method, params = {}) {
  const response = await fetch(rpcUrl, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: ++requestId, method, params }),
    signal: AbortSignal.timeout(10_000),
  })
  assert.ok(response.ok, `${method}: HTTP ${response.status}`)
  const body = await response.json()
  assert.ok(!body.error, `${method}: ${JSON.stringify(body.error)}`)
  return body.result
}
const head = async () => (await rpc('getFinalizedCblock')).consensusSlot
async function freeTcp(port) {
  const socket = net.createServer()
  await new Promise((resolve, reject) => { socket.once('error', reject); socket.listen({ host: '127.0.0.1', port, exclusive: true }, resolve) })
  await new Promise((resolve, reject) => socket.close(error => error ? reject(error) : resolve()))
}
async function freeUdp(port) {
  const socket = dgram.createSocket('udp4')
  try { await new Promise((resolve, reject) => { socket.once('error', reject); socket.bind({ address: '127.0.0.1', port, exclusive: true }, resolve) }) }
  finally { try { socket.close() } catch (error) { if (error.code !== 'ERR_SOCKET_DGRAM_NOT_RUNNING') throw error } }
}
function child(binary, args) {
  const process = spawn(binary, args, { cwd: runDir, env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true })
  children.add(process)
  process.once('error', error => { fatal = error })
  return process
}
async function stopChild(process) {
  if (!process?.pid || process.exitCode !== null || process.signalCode !== null) return
  const exited = new Promise(resolve => process.once('exit', resolve))
  process.kill('SIGTERM')
  await Promise.race([exited, pause(5000)])
  if (process.exitCode === null && process.signalCode === null) {
    process.kill('SIGKILL'); await Promise.race([exited, pause(5000)])
  }
  assert.ok(process.exitCode !== null || process.signalCode !== null, `Owned child ${process.pid} did not stop.`)
}
function observe(chunk) {
  const text = chunk.toString()
  observerTail = (observerTail + text).slice(-64_000)
  observerLine += text
  let newline
  while ((newline = observerLine.indexOf('\n')) >= 0) {
    const line = observerLine.slice(0, newline); observerLine = observerLine.slice(newline + 1)
    // A reconnect starts again from a fresh snapshot, so its first slots may repeat.
    if (line.startsWith('link lost:')) { linkLosses++; lastMatchedSlot = null }
    const slot = /^slot (\d+) re-executed\b/.exec(line)
    if (slot) {
      if (!/\bmatches\b/.test(line)) { fatal = new Error(`Observer state mismatch: ${line}`); continue }
      const number = Number(slot[1])
      if (lastMatchedSlot !== null && number <= lastMatchedSlot) { fatal = new Error(`Observer repeated or reversed a finalized slot: ${line}`); continue }
      matchedSlots.add(number); lastMatchedSlot = number
    }
    for (const sample of line.matchAll(/\bRTT ([0-9]+(?:\.[0-9]+)?) ms\b/g)) roundTrips.push(Number(sample[1]))
  }
}
async function probe(mode, identity) {
  const process = child(probeBinary, ['--mode', mode, '--net', rpcUrl, '--hub', 'FRA', '--identity', identity])
  let tail = '', lines = ''
  const results = []
  process.stdout.on('data', chunk => {
    tail = (tail + chunk).slice(-16_000); lines += chunk
    let newline
    while ((newline = lines.indexOf('\n')) >= 0) {
      const line = lines.slice(0, newline); lines = lines.slice(newline + 1)
      if (!line.trim()) continue
      try {
        const result = JSON.parse(line); results.push(result)
        if (result.event === 'propose_probe_authenticated') authenticatedSlowPeer = true
        if (result.event === 'propose_probe_dropped') droppedSlowPeer = true
      } catch { fatal = new Error(`Probe emitted invalid JSON: ${line}`) }
    }
  })
  process.stderr.on('data', chunk => { tail = (tail + chunk).slice(-16_000) })
  const completed = new Promise((resolve, reject) => process.once('exit', code => {
    if (code === 0) resolve(results)
    else reject(new Error(`${mode} probe failed (${code}): ${tail}`))
  }))
  // Attach a handler immediately: the outer check may still be measuring the live head.
  completed.catch(error => { fatal = error })
  return { process, completed }
}

try {
  // Every address is checked before anything starts. Existing listeners are never stopped.
  for (const port of [ports.rpc, ports.ws, ports.metrics]) await freeTcp(port)
  for (let port = ports.quic; port <= ports.external; port++) await freeUdp(port)
  await access(binary); await access(probeBinary)
  runDir = await mkdtemp(join(target, 'propose-observer-check-'))
  const genesisPath = join(runDir, 'genesis.json')
  const genesis = JSON.parse(await readFile(join(backend, 'chain/genesis/acrux-net.json'), 'utf8'))
  // Absolute executable paths keep this unique genesis independent of its temporary location.
  for (const program of genesis.programs) program.path = resolve(backend, 'chain', program.path)
  await writeFile(genesisPath, JSON.stringify(genesis))
  const identity = join(runDir, 'observer-keypair.json'), slowIdentity = join(runDir, 'slow-keypair.json'), badIdentity = join(runDir, 'bad-signature-keypair.json')
  await writeFile(identity, JSON.stringify([...Keypair.generate().secretKey]), { mode: 0o600 })
  await writeFile(slowIdentity, JSON.stringify([...Keypair.generate().secretKey]), { mode: 0o600 })
  await writeFile(badIdentity, JSON.stringify([...Keypair.generate().secretKey]), { mode: 0o600 })
  env = { ...process.env, ACRUX_BIND: '127.0.0.1', ACRUX_IDENTITY_SEED: '20261116', ACRUX_BRIDGE: join(runDir, 'no-bridge.json'), ACRUX_PLACEMENTS: join(runDir, 'no-placements.json') }
  for (const name of ['ACRUX_BRIDGE_RPC', 'ACRUX_LOG_RPC', 'ACRUX_STATE_WRITE_DELAY_MS', 'ACRUX_STATE_WRITE_FAIL_MS', 'ACRUX_PEER_PORT', 'ACRUX_PEER_ADDRESS']) delete env[name]
  netChild = child(binary, ['net', 'up', '--genesis', genesisPath, '--store', join(runDir, 'net'),
    '--gateway-port', String(ports.rpc), '--ws-port', String(ports.ws), '--metrics-port', String(ports.metrics),
    '--base-port', String(ports.quic), '--peer-port', String(ports.external), '--peer-address', `127.0.0.1:${ports.external}`, '--traffic-rate', '50'])
  for (const stream of [netChild.stdout, netChild.stderr]) stream.on('data', chunk => { netTail = (netTail + chunk).slice(-64_000) })
  await until('Local finalized head', 60_000, async () => {
    try { return await head() !== null }
    catch (error) { if (error.cause?.code === 'ECONNREFUSED') return false; throw error }
  })
  const peers = await rpc('getHubPeers', { hub: 'FRA' })
  assert.equal(peers.hub, 'FRA'); assert.equal(peers.address, `127.0.0.1:${ports.external}`)
  assert.ok(typeof peers.certificate === 'string' && peers.certificate.length > 0)
  assert.ok(peers.peers.length > 0 && peers.peers.every(peer => Number.isSafeInteger(peer.id) && typeof peer.identity === 'string'))
  if (far) {
    // 145 ms each way, then the answer at 1 MB/s: a multi-megabyte snapshot takes seconds, as it does live.
    const proxy = http.createServer((request, response) => {
      let body = ''
      request.on('data', chunk => { body += chunk })
      request.on('end', async () => {
        await pause(145)
        const answer = Buffer.from((await (await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body })).text())
          .replaceAll(`127.0.0.1:${ports.external}`, `127.0.0.1:${relayPort}`))
        await pause(145 + Math.ceil(answer.length / 1000))
        response.writeHead(200, { 'content-type': 'application/json' }).end(answer)
      })
    })
    await new Promise(resolve => proxy.listen(farPort, '127.0.0.1', resolve))
    farProxy = proxy
    // One client, one upstream: each datagram waits 145 ms before it is passed on, either way.
    const relay = dgram.createSocket('udp4'), upstream = dgram.createSocket('udp4')
    let client, relayOpen = true, heldUntil = 0
    const pass = (socket, message, port, address) => { if (relayOpen && Date.now() >= heldUntil) socket.send(message, port, address) }
    relay.on('message', (message, from) => { client = from; setTimeout(() => pass(upstream, message, ports.external, '127.0.0.1'), 145) })
    upstream.on('message', message => { if (client) setTimeout(() => pass(relay, message, client.port, client.address), 145) })
    await new Promise(resolve => relay.bind(relayPort, '127.0.0.1', resolve))
    farRelay = { close: () => { relayOpen = false; relay.close(); upstream.close() }, hold: ms => { heldUntil = Date.now() + ms } }
    event('propose_far_proxy', { url: farUrl, eachWayMs: 145, bytesPerSecond: 1_000_000, quicRelay: `127.0.0.1:${relayPort}` })
  }
  observer = child(binary, ['node', 'propose', '--hub', 'FRA', '--identity', identity, '--net', far ? farUrl : rpcUrl, '--genesis', genesisPath])
  observer.stdout.on('data', observe)
  observer.stderr.on('data', chunk => { observerTail = (observerTail + chunk).slice(-64_000) })
  const startedAt = Date.now()
  await until('Observer re-execution over its authenticated QUIC link', 60_000, async () => matchedSlots.size >= 20)
  event('propose_observer_started', { ports, netPid: netChild.pid, observerPid: observer.pid, matchedSlots: matchedSlots.size })

  const bad = await probe('bad-signature', badIdentity)
  const badResults = await Promise.race([bad.completed, pause(15_000).then(() => { throw new Error('Bad challenge probe deadline exceeded.') })])
  assert.ok(badResults.some(result => result.event === 'propose_probe_bad_signature_refused'))
  // The net logs each refusal with its reason (at most one line a second), so a live refusal can be read.
  const logged = await until('The logged refusal', 10_000, async () => netTail.split('\n').find(line => line.includes('external_observer_refused') && line.includes('identity proof rejected')))
  event('propose_bad_signature_refused', { logged: JSON.parse(logged.slice(logged.indexOf('{'))) })

  const slowStarted = Date.now()
  const slow = await probe('slow-reader', slowIdentity)
  await until('Slow peer authentication', 15_000, async () => authenticatedSlowPeer)
  const beforeSlow = await head(), matchedBeforeSlow = matchedSlots.size
  const paceStarted = Date.now()
  const pace = (async () => {
    // The host gives each observer write 10 s before it closes a peer that stopped reading.
    await pause(12_000)
    alive()
    const after = await head(), elapsedSeconds = (Date.now() - paceStarted) / 1000
    const finalizedSlotsAdvanced = (after - beforeSlow) * 4
    assert.ok(droppedSlowPeer, 'Bounded backpressure must drop the stopped-reading peer within the twelve-second pace window.')
    const finalizedSlotsPerSecond = finalizedSlotsAdvanced / elapsedSeconds
    assert.ok(finalizedSlotsPerSecond >= 12, `The net slowed to ${finalizedSlotsPerSecond.toFixed(2)} finalized slots/s during the slow-peer scenario.`)
    assert.ok(matchedSlots.size > matchedBeforeSlow, 'The healthy observer must keep matching during the slow-peer scenario.')
    return { elapsedSeconds, finalizedSlotsAdvanced, finalizedSlotsPerSecond, observerMatchesAdvanced: matchedSlots.size - matchedBeforeSlow }
  })()
  pace.catch(error => { fatal = error })
  const slowResults = await Promise.race([slow.completed, pause(45_000).then(() => { throw new Error('Stopped-reading peer was not dropped within 45 seconds.') })])
  assert.ok(droppedSlowPeer && slowResults.some(result => result.event === 'propose_probe_dropped'))
  const paced = await pace
  event('propose_slow_peer_dropped', { elapsedMs: Date.now() - slowStarted, ...paced })

  await until('One thousand finalized slot state hashes match', 150_000, async () => matchedSlots.size >= 1000)
  alive()
  assert.ok(roundTrips.length >= 4, 'Observer must report measured QUIC link RTT samples.')
  roundTrips.sort((a, b) => a - b)
  const medianRttMs = roundTrips[Math.floor(roundTrips.length / 2)]
  event('propose_link_rtt_measured', { samples: roundTrips.length, medianRttMs, minimumRttMs: roundTrips[0], simulatedFraRttMs: [2, 8], appliedDelayMs: 0 })
  if (far) assert.ok(medianRttMs >= 290 && medianRttMs < 400, `The relayed link's median RTT ${medianRttMs} ms must be the relay's 290 ms.`)
  else assert.ok(medianRttMs < 2, `Loopback link median RTT ${medianRttMs} ms must be below FRA's minimum simulated 2 ms RTT.`)
  if (far) {
    // The link goes silent for 15 s, past the host's 10 s write deadline; the observer reconnects.
    const before = matchedSlots.size, heldAt = Date.now()
    farRelay.hold(15_000)
    await until('The observer reports its lost link', 90_000, async () => linkLosses > 0)
    await until('The reconnected observer matches 50 more slots', 120_000, async () => matchedSlots.size >= before + 50)
    event('propose_observer_reconnected', { heldMs: 15_000, linkLosses, recoveredAfterMs: Date.now() - heldAt, matchedSlots: matchedSlots.size })
  }
  event('propose_observer_passed', { matchedSlots: matchedSlots.size, lastMatchedSlot, elapsedSeconds: (Date.now() - startedAt) / 1000, medianRttMs, badSignatureRefused: true, stoppedReadingPeerDropped: true })
} catch (error) {
  // The net's own account of each outside peer it closed or refused.
  for (const line of netTail.split(String.fromCharCode(10))) if (line.includes('external_observer')) console.error(line)
  throw error
} finally {
  farProxy?.close()
  farRelay?.close()
  // Stop each owned process before deleting its private key/store/log directory.
  const failures = []
  for (const process of [...children].reverse()) {
    try { await stopChild(process) } catch (error) { failures.push(error) }
  }
  if (runDir && failures.length === 0) {
    assert.ok(resolve(runDir).startsWith(resolve(target) + sep), 'Cleanup must stay within node/target.')
    await rm(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
  }
  if (failures.length) throw new AggregateError(failures, 'Owned child cleanup failed.')
}
