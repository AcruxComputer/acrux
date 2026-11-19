// Build 5 step 4, with the net's default traffic, without a bridge or an outside validator.
// Run after the coordinated node/API/contract builds: node scripts/hash-exchange-check.mjs
// Owns only its local net, API 5044 and a uniquely named temporary database on Postgres 4921.
// The exported helper lets the operator's native-exchange check exercise the same real API.
import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import { spawn, spawnSync } from 'node:child_process'
import dgram from 'node:dgram'
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import net from 'node:net'
import { dirname, join, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Pool } from 'pg'
import { Facts, Hub, HashExchangeTelemetry } from '@acrux/contract'
import { DEV_SEED } from '../../chain/scripts/dev-keys.mjs'

const apiDir = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const chainDir = resolve(apiDir, '../chain'), nodeDir = resolve(apiDir, '../node'), target = join(nodeDir, 'target')
const binary = join(target, 'release', process.platform === 'win32' ? 'acrux.exe' : 'acrux')
const gatewayUrl = 'http://127.0.0.1:5041', internalUrl = 'http://127.0.0.1:5043', apiUrl = 'http://127.0.0.1:5044'
const pause = ms => new Promise(resolve => setTimeout(resolve, ms))
const event = (name, fields) => console.log(JSON.stringify({ event: name, ...fields }))
const boundedFetch = (url, options = {}) => fetch(url, { ...options, signal: AbortSignal.timeout(10_000) })

export async function freeTcp(port) {
  const server = net.createServer()
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen({ port, host: '127.0.0.1', exclusive: true }, resolve) })
  await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()))
}
export async function stopOwned(child) {
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null) return
  const exited = new Promise(resolve => child.once('exit', resolve))
  child.kill('SIGTERM'); await Promise.race([exited, pause(5000)])
  if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await Promise.race([exited, pause(5000)]) }
  assert.ok(child.exitCode !== null || child.signalCode !== null, 'A check-owned child did not stop.')
}
async function until(label, seconds, read, healthy) {
  const deadline = Date.now() + seconds * 1000
  while (Date.now() < deadline) {
    healthy()
    const value = await read()
    if (value) return value
    await pause(500)
  }
  throw new Error(`${label}: deadline exceeded after ${seconds}s.`)
}
async function nodeRpc(method, params = {}) {
  const response = await boundedFetch(gatewayUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })
  assert.ok(response.ok, `${method}: HTTP ${response.status}`)
  const body = await response.json()
  if (body.error) { const error = new Error(`${method}: ${JSON.stringify(body.error)}`); error.rpcCode = body.error.data?.error?.code; throw error }
  return body.result
}
function stats(value) {
  assert.ok(value && Number.isSafeInteger(value.count) && value.count > 0)
  assert.ok(Number.isSafeInteger(value.p50) && value.p50 >= 0 && Number.isSafeInteger(value.p99) && value.p99 >= value.p50)
}
function validateTelemetry(value) {
  const telemetry = HashExchangeTelemetry.parse(value)
  assert.equal(telemetry.measurement, 'roundTrip')
  for (const hub of telemetry.hubs) {
    for (const pair of hub.pairs) { assert.equal(new Set(pair.members).size, 2); stats(pair) }
    for (const kind of ['applied', 'real']) if (hub.commitSpreadUs[kind] !== null) stats(hub.commitSpreadUs[kind])
  }
  for (const kind of ['applied', 'real']) if (telemetry.commitSpreadByLinkUs[kind] !== null) stats(telemetry.commitSpreadByLinkUs[kind])
  return telemetry
}
async function privateMetrics(url = internalUrl) {
  const response = await boundedFetch(`${url}/internal/hash-exchange`)
  assert.ok(response.ok, `Private hash-exchange metrics: HTTP ${response.status}`)
  return validateTelemetry(await response.json())
}
// The commit spread is the hosted validators' view; an outside member's own export carries its pairs.
function assertPrometheus(text, telemetry, wantedKind, spread = true) {
  assert.ok(text.includes('acrux_hash_exchange_rtt_microseconds{'))
  for (const hub of telemetry.hubs) for (const pair of hub.pairs.filter(pair => pair.kind === wantedKind)) {
    const labels = `hub="${hub.hubCode}",member_a="${pair.members[0]}",member_b="${pair.members[1]}",kind="${pair.kind}"`
    assert.ok(text.includes(`acrux_hash_exchange_rtt_microseconds{${labels},quantile="0.5"}`))
    assert.ok(text.includes(`acrux_hash_exchange_rtt_microseconds{${labels},quantile="0.99"}`))
    assert.ok(text.includes(`acrux_hash_exchange_samples{${labels}}`))
  }
  if (spread) assert.ok(text.includes('acrux_commit_spread_by_link_microseconds{'))
}

/** Real production API and its registry indexers, isolated from every existing local row. */
export async function checkExchangeApi({ outsideIdentity = null, outsideMetricsUrl = null, healthy = () => {} } = {}) {
  await freeTcp(5044)
  try { process.loadEnvFile(join(apiDir, '.env')) } catch (error) { if (error.code !== 'ENOENT') throw error }
  assert.ok(process.env.DATABASE_URL, 'Set the local DATABASE_URL in api/.env.')
  const base = new URL(process.env.DATABASE_URL)
  assert.ok(['127.0.0.1', 'localhost'].includes(base.hostname) && base.port === '4921', 'This check uses only local Postgres 4921.')
  const databaseName = `acrux_hash_exchange_check_${randomBytes(8).toString('hex')}`
  assert.match(databaseName, /^acrux_hash_exchange_check_[a-f0-9]{16}$/)
  const admin = new Pool({ connectionString: base.toString(), max: 1, statement_timeout: 10_000 })
  const databaseUrl = new URL(base); databaseUrl.pathname = `/${databaseName}`
  let created = false, api, apiTail = '', apiError
  const assertHealthy = () => {
    healthy()
    if (apiError) throw apiError
    if (api && (api.exitCode !== null || api.signalCode !== null)) throw new Error(`The check-owned API exited: ${apiTail.slice(-6000)}`)
  }
  try {
    await admin.query(`CREATE DATABASE "${databaseName}"`); created = true
    const migrated = spawnSync(process.execPath, [join(apiDir, 'node_modules/prisma/build/index.js'), 'migrate', 'deploy'], {
      cwd: apiDir, env: { ...process.env, DATABASE_URL: databaseUrl.toString() }, windowsHide: true, encoding: 'utf8', timeout: 60_000,
    })
    assert.equal(migrated.status, 0, `Migrate the check-owned database: ${migrated.stderr}`)
    api = spawn(process.execPath, ['dist/server.js'], {
      cwd: apiDir, env: { ...process.env, MODE: 'node', HOST: '127.0.0.1', PORT: '5044', DATABASE_URL: databaseUrl.toString(),
        GATEWAY_RPC_URL: gatewayUrl, GATEWAY_INTERNAL_URL: internalUrl, SOLANA_RPC_URL: gatewayUrl,
        BRIDGE_PROGRAM_ID: '', BRIDGE_FROM_SLOT: '', ACRUX_MINT: '', REGISTRY_CLUSTER: 'localnet', REGISTRY_POLL_SECONDS: '2',
        REGISTRY_PROGRAM_ID: 'ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm', ACRUX_BIN: binary, ACRUX_NODE_DIR: nodeDir,
      }, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
    })
    api.once('error', error => { apiError = error })
    for (const stream of [api.stdout, api.stderr]) stream.on('data', data => { apiTail = (apiTail + data).slice(-64_000) })
    const readApi = async path => {
      let response
      try { response = await boundedFetch(`${apiUrl}${path}`) }
      catch (error) { if (error.cause?.code === 'ECONNREFUSED') return null; throw error }
      const body = await response.json()
      if (response.status === 503 && ['PROGRAM_UNAVAILABLE', 'NET_UNAVAILABLE'].includes(body.error?.code)) return null
      if (response.status === 404 && body.error?.code === 'NOT_FOUND') return null
      assert.ok(response.ok, `${path}: ${JSON.stringify(body)}`)
      return body
    }
    const ready = await until('Measured pairs and the actual API registry join', 150, async () => {
      const raw = await privateMetrics()
      const applied = raw.hubs.find(hub => hub.hubCode === 'FRA')?.pairs.filter(pair => pair.kind === 'applied') ?? []
      if (!['FRA', 'NYC', 'TYO'].every(code => raw.hubs.some(hub => hub.hubCode === code && hub.pairs.some(pair => pair.kind === 'applied' && pair.count >= 3)))) return false
      if (outsideIdentity && !raw.hubs.find(hub => hub.hubCode === 'FRA')?.pairs.some(pair => pair.kind === 'real' && pair.members.includes(outsideIdentity) && pair.count >= 3)) return false
      if (!raw.commitSpreadByLinkUs.applied || outsideIdentity && !raw.commitSpreadByLinkUs.real) return false
      const [factsBody, hubBody] = await Promise.all([readApi('/v1/facts'), readApi('/v1/hubs/FRA?chain=acrux')])
      if (!factsBody || !hubBody) return false
      const facts = Facts.parse(factsBody), hub = Hub.parse(hubBody)
      if (!hub.hashExchange?.pairs.length || !facts.hashExchange?.hubs.length) return false
      if (outsideIdentity && !hub.members.some(member => member.identity === outsideIdentity && member.validatorKind === 'external')) return false
      if (outsideIdentity && !hub.hashExchange.pairs.some(pair => pair.kind === 'real' && pair.members.includes(outsideIdentity))) return false
      return { raw, facts, hub, applied }
    }, assertHealthy)
    const { raw, facts, hub } = ready
    assert.equal(facts.hashExchange.measurement, 'roundTrip'); assert.equal(hub.hashExchange.measurement, 'roundTrip')
    assert.ok(facts.commitSpreadUs && Number.isSafeInteger(facts.commitSpreadUs.p50), 'The legacy unsplit spread remains available.')
    stats(facts.commitSpreadByLinkUs.applied)
    const members = new Set(hub.members.filter(member => BigInt(member.proposerStake) > 0n).map(member => member.identity))
    assert.ok(hub.hashExchange.pairs.every(pair => pair.members.every(identity => members.has(identity))), 'The API joins pairs to current effective members.')
    const factHub = facts.hashExchange.hubs.find(item => item.hubCode === 'FRA')
    assert.ok(factHub)
    assert.deepEqual(factHub.pairs, hub.hashExchange.pairs, 'Concurrent Facts and hub reads use the same validated private sample cache.')
    const prometheus = await (await boundedFetch(`${internalUrl}/metrics`)).text()
    assertPrometheus(prometheus, raw, outsideIdentity ? 'real' : 'applied')
    // Keep reading through multiple private-cache expirations while traffic and the API's
    // actual registry indexers run. A responsive but frozen finalized head is a failure too.
    const initialHead = (await nodeRpc('getFinalizedCblock')).consensusSlot
    assert.ok(Number.isSafeInteger(initialHead))
    const readingStarted = Date.now()
    let lastHead = initialHead, lastProgress = Date.now(), readBatches = 0, maximumReadMs = 0
    while (Date.now() - readingStarted < 15_000) {
      assertHealthy()
      const started = Date.now()
      const [head, privateBody, factsBody, hubBody] = await Promise.all([
        nodeRpc('getFinalizedCblock'), privateMetrics(), readApi('/v1/facts'), readApi('/v1/hubs/FRA?chain=acrux'),
      ])
      const readMs = Date.now() - started
      maximumReadMs = Math.max(maximumReadMs, readMs)
      assert.ok(readMs < 5000, 'Gateway and API reads must remain responsive under default traffic.')
      assert.ok(factsBody && hubBody, 'Ready Facts and hub routes must stay available under traffic.')
      Facts.parse(factsBody); Hub.parse(hubBody)
      assert.ok(privateBody.hubs.length > 0, 'Measured hub data must remain available throughout the API reads.')
      assert.ok(Number.isSafeInteger(head.consensusSlot) && head.consensusSlot >= lastHead)
      if (head.consensusSlot > lastHead) { lastHead = head.consensusSlot; lastProgress = Date.now() }
      assert.ok(Date.now() - lastProgress < 5000, 'The net stopped finalizing while serving real API reads.')
      readBatches++
      await pause(500)
    }
    assert.ok(lastHead > initialHead, 'Default traffic must keep finalizing throughout the API check.')
    event('hash_exchange_api_remained_responsive', { elapsedSeconds: (Date.now() - readingStarted) / 1000,
      readBatches, maximumReadMs, finalizedSlotsAdvanced: (lastHead - initialHead) * 4 })
    if (outsideIdentity) {
      const applied = hub.hashExchange.pairs.filter(pair => pair.kind === 'applied')
      const real = hub.hashExchange.pairs.filter(pair => pair.kind === 'real' && pair.members.includes(outsideIdentity))
      assert.ok(applied.length && real.length)
      const appliedMedian = applied.map(pair => pair.p50).sort((a, b) => a - b)[Math.floor(applied.length / 2)]
      const realMedian = real.map(pair => pair.p50).sort((a, b) => a - b)[Math.floor(real.length / 2)]
      assert.ok(realMedian * 2 < appliedMedian, `Loopback real RTT ${realMedian}us must stay well below applied FRA RTT ${appliedMedian}us.`)
      stats(facts.commitSpreadByLinkUs.real); stats(hub.hashExchange.commitSpreadUs.real)
      assert.ok(outsideMetricsUrl, 'The outside check supplies its own metrics listener.')
      const own = await privateMetrics(outsideMetricsUrl)
      assert.ok(own.hubs.some(item => item.pairs.some(pair => pair.kind === 'real' && pair.members.includes(outsideIdentity))))
      assertPrometheus(await (await boundedFetch(`${outsideMetricsUrl}/metrics`)).text(), own, 'real', false)
      event('native_hash_exchange_check_passed', { identity: outsideIdentity, appliedRoundTripUs: appliedMedian, realRoundTripUs: realMedian,
        pairs: hub.hashExchange.pairs, commitSpreadByLinkUs: facts.commitSpreadByLinkUs })
    } else {
      assert.ok(raw.hubs.every(item => item.pairs.every(pair => pair.kind === 'applied')))
      assert.equal(raw.commitSpreadByLinkUs.real, null); assert.equal(facts.commitSpreadByLinkUs.real, null)
      assert.ok(raw.hubs.every(item => item.commitSpreadUs.real === null))
      assert.ok(!/acrux_hash_exchange_\w+\{[^\n]*kind="real"/.test(prometheus), 'Unmeasured real links have no synthetic zero series.')
      const solanaHub = Hub.parse(await until('The local-only Solana registry view', 30, () => readApi('/v1/hubs/FRA?chain=solana'), assertHealthy))
      assert.equal(solanaHub.hashExchange, undefined)
      event('hash_exchange_api_check_passed', { hubs: raw.hubs.map(item => ({ hubCode: item.hubCode, pairs: item.pairs.length })),
        measurement: facts.hashExchange.measurement, commitSpreadByLinkUs: facts.commitSpreadByLinkUs })
    }
    return ready
  } finally {
    const failures = []
    try { await stopOwned(api) } catch (error) { failures.push(error) }
    if (created && !failures.length) {
      try {
        assert.match(databaseName, /^acrux_hash_exchange_check_[a-f0-9]{16}$/)
        assert.ok(['127.0.0.1', 'localhost'].includes(base.hostname) && base.port === '4921')
        await admin.query(`DROP DATABASE "${databaseName}"`)
      } catch (error) { failures.push(error) }
    }
    try { await admin.end() } catch (error) { failures.push(error) }
    if (failures.length) throw new AggregateError(failures, 'Check-owned API/database cleanup failed.')
  }
}

async function main() {
  let runDir, child, tail = '', childError, interrupted = false
  process.once('SIGINT', () => { interrupted = true }); process.once('SIGTERM', () => { interrupted = true })
  const healthy = () => {
    if (interrupted) throw new Error('Hash-exchange check interrupted.')
    if (childError) throw childError
    if (child && (child.exitCode !== null || child.signalCode !== null)) throw new Error(`The check-owned net exited: ${tail.slice(-6000)}`)
  }
  try {
    for (const port of [5041, 5042, 5043, 5044]) await freeTcp(port)
    for (let port = 5062; port <= 5074; port++) {
      const socket = dgram.createSocket('udp4')
      try { await new Promise((resolve, reject) => { socket.once('error', reject); socket.bind({ port, address: '127.0.0.1', exclusive: true }, resolve) }) }
      finally { try { socket.close() } catch (error) { if (error.code !== 'ERR_SOCKET_DGRAM_NOT_RUNNING') throw error } }
    }
    runDir = mkdtempSync(join(target, 'hash-exchange-check-')); mkdirSync(join(runDir, 'genesis'))
    const env = { ...process.env, ACRUX_GENESIS_SEED: DEV_SEED, ACRUX_BIND: '127.0.0.1' }
    for (const name of ['ACRUX_BRIDGE', 'ACRUX_PLACEMENTS', 'ACRUX_IDENTITY_SEED', 'ACRUX_STATE_WRITE_DELAY_MS', 'ACRUX_STATE_WRITE_FAIL_MS', 'ACRUX_LOG_RPC', 'ACRUX_TRAFFIC_RATE']) delete env[name]
    env.ACRUX_IDENTITY_SEED = randomBytes(32).toString('hex')
    const genesis = join(runDir, 'genesis/acrux-net.json')
    const generated = spawnSync(process.execPath, ['scripts/genesis.mjs', genesis], { cwd: chainDir, env, windowsHide: true, encoding: 'utf8', timeout: 30_000 })
    assert.equal(generated.status, 0, 'Generate the check-owned local genesis.')
    const state = JSON.parse(readFileSync(genesis, 'utf8'))
    for (const entry of state.programs) entry.path = resolve(chainDir, entry.path)
    writeFileSync(genesis, JSON.stringify(state))
    child = spawn(binary, ['net', 'up', '--genesis', genesis, '--store', join(runDir, 'net'), '--gateway-port', '5041', '--ws-port', '5042', '--metrics-port', '5043', '--base-port', '5062', '--peer-port', '5074', '--peer-address', '127.0.0.1:5074'],
      { cwd: runDir, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
    child.once('error', error => { childError = error })
    for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { tail = (tail + data).slice(-64_000) })
    await until('The check-owned local net', 60, async () => {
      try { return (await nodeRpc('getFinalizedCblock')).consensusSlot !== null }
      catch (error) { if (error.cause?.code === 'ECONNREFUSED') return false; throw error }
    }, healthy)
    await pause(15_000)
    healthy()
    const warmedHead = (await nodeRpc('getFinalizedCblock')).consensusSlot
    assert.ok(Number.isSafeInteger(warmedHead))
    await until('The default-traffic net remains responsive after fifteen seconds', 5, async () =>
      (await nodeRpc('getFinalizedCblock')).consensusSlot > warmedHead, healthy)
    event('hash_exchange_default_traffic_warmed', { warmupSeconds: 15, finalizedCblock: warmedHead })
    await checkExchangeApi({ healthy })
  } catch (error) { console.error(`The check-owned net's last output:\n${tail}`); throw error }
  finally {
    await stopOwned(child)
    if (runDir) { assert.ok(resolve(runDir).startsWith(resolve(target) + sep)); rmSync(runDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }) }
  }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main()
