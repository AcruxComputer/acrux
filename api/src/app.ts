import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { z } from 'zod'
import { streamSSE } from 'hono/streaming'
import {
  Address, ApiClientError, Balance, Signature, CONTRACT_VERSION, FaucetRequest, LabRunRequest, PageQuery,
  HubCode, SCENARIOS, TxId, createGatewayClient, errors, paginate, BridgeTransfersQuery,
  type LabRun, type ProtocolEvent, type Transaction,
} from '@acrux/contract'
import {
  facts, genesisShare, genesisShareGenesisOnly, rules, rulesNone, hubEarnings, outsideHubEarnings, hubEarningsEmpty, faucetGrant, faucetStatus, fixtureErrors, hubs, labRuns, labRunsByState, labRunsByProtocol, labLanesCrashRun,
  makeFixtureTransaction, makeLabReplay, makeSchedulePreview, registryConfig, scenarios, externalBlockTransaction,
  specMap, transactionsByState, replayRange, replaySlot, burnNoEpochs, burnPending, burnBurned, burnRunning, bridgeFixture, bridgeWaitingFixture, bridgeTransfersFixture,
} from '@acrux/contract/fixtures'
import { createApi } from './engines/server/app.js'
import { emptyReplayRange } from './replay.js'
import { ApiFailure, fail } from './engines/server/errors.js'
import { fixtureScenario } from './engines/server/fixture.js'
import { readJson, readParam, readQuery } from './engines/server/input.js'
import { clientIp } from './engines/server/limits.js'
import { env, faucetSettings, registrySettings, unresolved } from './config.js'
import { FixtureLab } from './lab.js'
import { connectDatabase } from './db.js'
import { emptyBuyback, readBuyback } from './buyback/read.js'

type Scenario = typeof SCENARIOS[number]
const epochQuery = z.strictObject({ epoch: z.coerce.number().int().nonnegative().optional() })
const runId = z.string().min(1).max(128)

export function createFixtureApi() {
  const lab = new FixtureLab()
  const database = env.DATABASE_URL ? connectDatabase(env.DATABASE_URL) : null
  const grants: { address: string; ip: string; at: number }[] = []
  const gateway = createGatewayClient({ rpcUrl: env.GATEWAY_RPC_URL, wsUrl: 'ws://127.0.0.1:4931' })
  const app = createApi<{ scenario: Scenario | null }>({ origins: env.CORS_ORIGINS, contractVersion: CONTRACT_VERSION })
  if (env.MODE === 'fixture') app.use('*', fixtureScenario({ scenarios: SCENARIOS, errors: fixtureErrors, status: errors.status }))
  else app.use('*', async (c, next) => { c.set('scenario', null); await next() })
  app.get('/health', async c => {
    let status: 'fixture' | 'waiting' | 'ready' = 'fixture'
    if (env.MODE !== 'fixture') {
      status = 'waiting'
      try {
        const response = await fetch(new URL('/health', env.GATEWAY_RPC_URL), { signal: AbortSignal.timeout(2000) })
        const health = await response.json() as { status?: string }
        if (response.ok && health.status === 'ready') status = 'ready'
      } catch { /* An unavailable node is waiting; no synthetic readiness. */ }
    } else if (c.get('scenario') === 'waiting') status = 'waiting'
    return c.json({ status, contractVersion: CONTRACT_VERSION,
      missingConfiguration: [...unresolved(registrySettings), ...unresolved(faucetSettings)] })
  })
  app.get('/openapi.json', c => c.json(JSON.parse(readFileSync(fileURLToPath(import.meta.resolve('@acrux/contract/openapi.json')), 'utf8'))))
  app.get('/v1/buyback', async c => {
    const settings = { mint: env.ACRUX_MINT, wallet: env.LAUNCH_WALLET }
    if (!settings.mint) return c.json(emptyBuyback(settings))
    if (!database) fail('CONFIGURATION_REQUIRED', 'Configure DATABASE_URL before reading buyback history.', { stage: 'read' })
    return c.json(await readBuyback(database.db, settings))
  })
  app.get('/v1/burn', c => {
    const scenario = c.get('scenario'), query = readQuery(c, PageQuery)
    const source = scenario === 'empty' || scenario === 'burn-no-epochs' ? burnNoEpochs
      : scenario === 'burn-pending' ? burnPending : scenario === 'burn-burned' ? burnBurned : burnRunning
    const rows = source.epochs.items.filter(row => query.cursor === undefined || row.epoch < Number(query.cursor))
    const items = rows.slice(0, query.limit), hasMore = rows.length > items.length
    return c.json({ ...source, epochs: { ...source.epochs, items, page: { nextCursor: hasMore ? String(items.at(-1)!.epoch) : null, hasMore } } })
  })
  app.get('/v1/bridge', c => c.json(c.get('scenario') === 'empty' ? bridgeWaitingFixture : bridgeFixture))
  app.get('/v1/bridge/transfers', c => {
    const query = readQuery(c, BridgeTransfersQuery)
    const rows = c.get('scenario') === 'empty' ? [] : bridgeTransfersFixture.filter(t => (!query.kind || t.kind === query.kind) && (!query.address || t.from === query.address || t.to === query.address))
    return c.json(paginate(rows, query))
  })
  app.get('/v1/replay', c => c.json(c.get('scenario') === 'empty' ? emptyReplayRange() : replayRange))
  app.get('/v1/replay/:slot', c => {
    if (readParam(c, 'slot', z.coerce.number().int().nonnegative()) !== replaySlot.slot) fail('NOT_FOUND', 'This mainnet slot has not been replayed.')
    return c.json(replaySlot)
  })
  app.get('/v1/hubs', c => {
    const scenario = c.get('scenario')
    const rows = scenario === 'empty' || scenario === 'waiting' ? [] : scenario === 'pending_only' ? hubs.filter(h => h.status === 'pending') : hubs
    return c.json(paginate(rows, readQuery(c, PageQuery)))
  })
  app.get('/v1/genesis-share', c => c.json(c.get('scenario') === 'empty' ? genesisShareGenesisOnly : genesisShare))
  app.get('/v1/rules', c => c.json(c.get('scenario') === 'empty' ? rulesNone : rules))
  app.get('/v1/hubs/:code/earnings', c => {
    const code = readParam(c, 'code', HubCode)
    readQuery(c, z.strictObject({}))
    const earnings = c.get('scenario') === 'empty' ? hubEarningsEmpty : code === 'SIN' ? outsideHubEarnings : hubEarnings
    return c.json({ ...earnings, hubCode: code, epochs: earnings.epochs.map(epoch => ({ ...epoch, rows: earnings.hubCode === code ? epoch.rows : [] })) })
  })
  app.get('/v1/hubs/:code', c => {
    const code = readParam(c, 'code', HubCode)
    const found = hubs.find(h => h.code === code)
    if (!found) fail('NOT_FOUND', 'This hub was not found.')
    const state = c.get('scenario')
    const row = state === 'active' || state === 'inactive' || state === 'pending' ? hubs.find(h => h.status === state)! : found
    return c.json({ ...row, code })
  })
  app.get('/v1/registry/config', c => c.json({ ...registryConfig,
    status: env.MODE === 'fixture' && c.get('scenario') !== 'waiting' ? 'fixture' : 'waiting',
    programId: env.REGISTRY_PROGRAM_ID ?? null,
    depositLamports: env.REGISTRY_DEPOSIT_LAMPORTS ?? null,
    registrationFeeBps: env.REGISTRY_REGISTRATION_FEE_BPS ?? null,
    cooldownEpochs: env.REGISTRY_COOLDOWN_EPOCHS ?? null, inactivityEpochs: env.REGISTRY_INACTIVITY_EPOCHS ?? null,
    codeRules: { pattern: env.REGISTRY_CODE_PATTERN ?? null, minLength: env.REGISTRY_CODE_MIN_LENGTH ?? null, maxLength: env.REGISTRY_CODE_MAX_LENGTH ?? null },
    minimumMembers: env.REGISTRY_MIN_MEMBERS ?? null, minimumStake: env.REGISTRY_MIN_STAKE ?? null,
    missingConfiguration: unresolved(registrySettings),
  }))
  app.get('/v1/registry/members/:identity', c => {
    const identity = readParam(c, 'identity', Address)
    const member = hubs.flatMap(h => h.members).find(m => m.identity === identity)
    if (!member) fail('NOT_FOUND', 'This validator has no hub membership.')
    return c.json(member)
  })
  app.get('/v1/schedule', c => {
    const query = readQuery(c, epochQuery)
    const schedule = makeSchedulePreview(query.epoch)
    return c.json(c.get('scenario') === 'empty' || c.get('scenario') === 'waiting' ? { ...schedule, totalWindows: 0, shares: [], windows: [], regionalRuns: [] } : schedule)
  })
  app.get('/v1/facts', c => c.json(c.get('scenario') === 'empty' ? { ...facts, sampleSize: 0 } : facts))
  app.get('/v1/spec', c => c.json(specMap))
  app.get('/v1/transactions/:id', async c => {
    const id = readParam(c, 'id', TxId), state = c.get('scenario')
    if (id === externalBlockTransaction.header.id) return c.json(externalBlockTransaction)
    if (state && Object.hasOwn(transactionsByState, state)) return c.json(makeFixtureTransaction(state as Transaction['state'], 64, id))
    if (id === transactionsByState.finalized.header.id) return c.json(transactionsByState.finalized)
    try { return c.json(await gateway.call('getTransaction', { id })) }
    catch (error) {
      if (error instanceof ApiClientError) throw new ApiFailure(error.status, error.body)
      fail('NET_UNAVAILABLE', 'Acrux Net is unavailable.', { retryable: true, stage: 'read' })
    }
  })
  app.get('/v1/faucet/:address', c => {
    const address = readParam(c, 'address', Address), limited = c.get('scenario') === 'limit_reached'
    const missing = unresolved(faucetSettings)
    if (missing.length) return c.json(faucetStatus(address, limited))
    const now = Date.now(), period = env.FAUCET_WINDOW_SECONDS! * 1000
    const live = grants.filter(g => now - g.at < period)
    const ip = clientIp(c)
    const remainingForKey = Math.max(0, env.FAUCET_PER_KEY_LIMIT! - live.filter(g => g.address === address).length)
    const remainingForIp = Math.max(0, env.FAUCET_PER_IP_LIMIT! - live.filter(g => g.ip === ip).length)
    return c.json({ address, policy: { amountLamports: env.FAUCET_GRANT_LAMPORTS!, perKey: env.FAUCET_PER_KEY_LIMIT!, perIp: env.FAUCET_PER_IP_LIMIT!, periodSeconds: env.FAUCET_WINDOW_SECONDS!, missingConfiguration: [] }, remainingForKey, remainingForIp,
      resetsAt: new Date(now + period).toISOString(), eligible: remainingForKey > 0 && remainingForIp > 0 && !limited, availableLamports: null })
  })
  app.post('/v1/faucet', async c => {
    const { address } = await readJson(c, FaucetRequest), state = c.get('scenario')
    if (state === 'limit_reached') fail('FAUCET_LIMIT', 'The faucet limit for this key or IP has been reached.', { retryable: true, retryAfterSeconds: 60, stage: 'send' })
    const missing = unresolved(faucetSettings)
    if (missing.length && state !== 'normal') fail('CONFIGURATION_REQUIRED', `Configure ${missing.join(', ')} before enabling the faucet.`, { stage: 'send' })
    const synthetic = faucetGrant(address), now = Date.now(), period = (env.FAUCET_WINDOW_SECONDS ?? 0) * 1000
    const ip = clientIp(c)
    if (!missing.length) {
      const live = grants.filter(g => now - g.at < period)
      if (live.filter(g => g.address === address).length >= env.FAUCET_PER_KEY_LIMIT! || live.filter(g => g.ip === ip).length >= env.FAUCET_PER_IP_LIMIT!) fail('FAUCET_LIMIT', 'The faucet limit for this key or IP has been reached.', { retryable: true, retryAfterSeconds: env.FAUCET_WINDOW_SECONDS, stage: 'send' })
    }
    const amountLamports = env.FAUCET_GRANT_LAMPORTS ?? synthetic.amountLamports
    let response: Response
    try { response = await fetch(new URL('/internal/fixture/faucet', env.GATEWAY_RPC_URL), { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Fixture': 'normal' }, body: JSON.stringify({ address, amountLamports }) }) }
    catch { fail('NET_UNAVAILABLE', 'Start the configured gateway before funding this key.', { retryable: true, stage: 'send' }) }
    if (!response.ok) fail('NET_UNAVAILABLE', 'The gateway could not submit this funding grant.', { retryable: true, stage: 'send' })
    const result: unknown = await response.json()
    if (env.MODE === 'node') {
      const grant = z.strictObject({ transactionId: TxId, signature: Signature, balance: Balance }).parse(result)
      grants.push({ address, ip, at: now })
      return c.json({ id: grant.transactionId, transactionId: grant.transactionId, address, amountLamports,
        signature: grant.signature, balance: grant.balance, grantedAt: new Date(now).toISOString() })
    }
    const balance = Balance.parse(result)
    grants.push({ address, ip, at: now })
    return c.json({ ...synthetic, id: `fixture-grant-${grants.length}`, amountLamports, balance, grantedAt: new Date(now).toISOString() })
  })
  app.get('/v1/lab/scenarios', c => c.json(c.get('scenario') === 'empty' ? [] : scenarios))
  app.get('/v1/lab/runs', c => {
    const state = c.get('scenario')
    const rows = state === 'empty' ? [] : state && Object.hasOwn(labRunsByState, state) ? [labRunsByState[state as LabRun['status']]] : lab.list()
    return c.json(paginate(rows, readQuery(c, PageQuery)))
  })
  app.post('/v1/lab/runs', async c => {
    const input = await readJson(c, LabRunRequest)
    const template = input.scenario === 'split-commitments'
      ? labRunsByProtocol[input.protocol]
      : input.protocol === 'lanes' && input.scenario === 'crash' ? labLanesCrashRun : labRuns.find(run => run.scenario.id === input.scenario)!
    return c.json(lab.start(template, input.seed ?? template.seed, c.get('scenario') === 'failed_to_start', input.protocol), 201)
  })
  const readRun = (id: string, scenario: Scenario | null) => {
    const record = lab.get(id)
    if (!record) fail('NOT_FOUND', 'This lab run was not found.')
    if (scenario && Object.hasOwn(labRunsByState, scenario)) return { ...record, run: { ...labRunsByState[scenario as LabRun['status']], id } }
    return record
  }
  app.get('/v1/lab/runs/:id', c => c.json(readRun(readParam(c, 'id', runId), c.get('scenario')).run))
  app.get('/v1/lab/runs/:id/events', c => {
    const record = readRun(readParam(c, 'id', runId), c.get('scenario'))
    return c.json({ run: record.run, events: record.events })
  })
  app.get('/v1/lab/runs/:id/stream', c => {
    const record = readRun(readParam(c, 'id', runId), c.get('scenario'))
    c.header('X-Accel-Buffering', 'no')
    return streamSSE(c, async stream => {
      let stopped = false
      const queue: ProtocolEvent[] = []
      let wake: (() => void) | undefined
      const listener = (event: ProtocolEvent) => { queue.push(event); wake?.() }
      record.listeners.add(listener)
      stream.onAbort(() => { stopped = true; record.listeners.delete(listener); wake?.() })
      const send = (event: ProtocolEvent) => stream.writeSSE({ id: String(event.sequence), event: event.type, data: JSON.stringify(event) })
      try {
        const last = c.req.header('Last-Event-ID')
        const replay = last === undefined ? record.events : record.events.filter(event => event.sequence > Number(last))
        for (const event of replay) { if (stopped) return; await send(event) }
        while (!stopped && (record.run.status === 'queued' || record.run.status === 'running' || queue.length)) {
          if (queue.length) { await send(queue.shift()!); continue }
          await new Promise<void>(resolve => { wake = resolve })
          wake = undefined
        }
      } finally { record.listeners.delete(listener) }
    })
  })
  return { app, close: async () => { lab.close(); await database?.close() } }
}
