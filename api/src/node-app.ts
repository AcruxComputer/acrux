// The API in node mode: every route answers from Acrux Net (the gateway), the Hub Registry (the
// registry read model in Postgres) or the Fault Lab runner, through the contract's schemas.
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { z } from 'zod'
import { streamSSE } from 'hono/streaming'
import {
  Address, ApiClientError, Balance, CONTRACT_VERSION, FaucetRequest, HubCode, LabRunRequest, PageQuery, Signature, SpecMap,
  TxId, createGatewayClient, paginate, type ProtocolEvent, type RpcMethod,
} from '@acrux/contract'
import { scenarios } from '@acrux/contract/fixtures'
import { createApi } from './engines/server/app.js'
import { ApiFailure, fail } from './engines/server/errors.js'
import { readJson, readParam, readQuery } from './engines/server/input.js'
import { clientIp, createWindows } from './engines/server/limits.js'
import { env } from './config.js'
import { connectDatabase } from './db.js'
import { NodeLab } from './node-lab.js'
import { RegistryIndexer, hubView, memberView, regionOf, totalStake } from './registry.js'
import { schedulePreview } from './schedule.js'

const epochQuery = z.strictObject({ epoch: z.coerce.number().int().nonnegative().optional() })
const runId = z.string().uuid()
const specPath = fileURLToPath(new URL('../../spec/map.json', import.meta.url))

export function createNodeApi() {
  const database = connectDatabase(env.DATABASE_URL!)
  const db = database.db
  const gateway = createGatewayClient({ rpcUrl: env.GATEWAY_RPC_URL, wsUrl: env.GATEWAY_RPC_URL.replace(/^http/, 'ws').replace(/:(\d+)/, (_, p) => `:${Number(p) + 1}`) })
  const registry = new RegistryIndexer(db, env.SOLANA_RPC_URL, env.REGISTRY_PROGRAM_ID!, env.REGISTRY_POLL_SECONDS * 1000)
  const lab = new NodeLab(db)
  registry.start()
  const app = createApi({ origins: env.CORS_ORIGINS, allowHeaders: ['Last-Event-ID'], contractVersion: CONTRACT_VERSION })

  const net = async <M extends RpcMethod>(method: M, params?: unknown) => {
    try { return await gateway.call(method, params as never) }
    catch (error) {
      if (error instanceof ApiClientError) throw new ApiFailure(error.status, error.body)
      fail('NET_UNAVAILABLE', 'Acrux Net is unavailable.', { retryable: true, stage: 'read' })
    }
  }
  const readyRegistry = () => {
    if (!registry.state.readAt) fail('PROGRAM_UNAVAILABLE', 'The Hub Registry has not been read yet.', { retryable: true, stage: 'read' })
    return registry.configView()
  }
  const hubRows = () => db.hub.findMany({ include: { members: { orderBy: { stakeLamports: 'desc' } } }, orderBy: { code: 'asc' } })

  app.get('/health', async c => {
    let status: 'waiting' | 'ready' = 'waiting'
    try {
      const response = await fetch(new URL('/health', env.GATEWAY_RPC_URL), { signal: AbortSignal.timeout(2000) })
      const health = await response.json() as { status?: string }
      if (response.ok && health.status === 'ready') status = 'ready'
    } catch { /* An unreachable node is waiting. */ }
    return c.json({ status, contractVersion: CONTRACT_VERSION, missingConfiguration: registry.state.readAt ? [] : ['Hub Registry RPC'] })
  })
  app.get('/openapi.json', c => c.json(JSON.parse(readFileSync(fileURLToPath(import.meta.resolve('@acrux/contract/openapi.json')), 'utf8'))))

  app.get('/v1/hubs', async c => {
    const config = readyRegistry()
    return c.json(paginate((await hubRows()).map(row => hubView(row, config)), readQuery(c, PageQuery)))
  })
  app.get('/v1/hubs/:code', async c => {
    const code = readParam(c, 'code', HubCode), config = readyRegistry()
    const row = await db.hub.findUnique({ where: { code }, include: { members: { orderBy: { stakeLamports: 'desc' } } } })
    if (!row) fail('NOT_FOUND', 'This hub was not found.')
    return c.json(hubView(row, config))
  })
  app.get('/v1/registry/config', c => c.json(registry.configView()))
  app.get('/v1/registry/members/:identity', async c => {
    const identity = readParam(c, 'identity', Address)
    const member = await db.member.findUnique({ where: { identity } })
    if (!member) fail('NOT_FOUND', 'This validator has no hub membership.')
    return c.json(memberView(member))
  })
  app.get('/v1/schedule', async c => {
    const config = readyRegistry(), { epoch = config.currentEpoch + 1 } = readQuery(c, epochQuery)
    const hubs = (await hubRows()).map(row => ({ code: row.code, region: regionOf(row.latitude, row.longitude),
      members: row.members.filter(m => BigInt(m.proposerStakeLamports.toString()) > 0n).map(m => ({ identity: m.identity, stake: Number(BigInt(m.stakeLamports.toString()) / 1_000_000_000n) })) }))
    return c.json(await schedulePreview(epoch, hubs, Number(totalStake / 1_000_000_000n)))
  })

  app.get('/v1/facts', async c => c.json(await net('getFacts', {})))
  app.get('/v1/spec', c => c.json(SpecMap.parse(JSON.parse(readFileSync(specPath, 'utf8')))))
  app.get('/v1/transactions/:id', async c => c.json(await net('getTransaction', { id: readParam(c, 'id', TxId) })))

  // The faucet: per-key and per-IP grants inside a rolling window, kept in Postgres.
  const window = () => new Date(Date.now() - env.FAUCET_WINDOW_SECONDS! * 1000)
  const ipHash = (ip: string) => createHash('sha256').update(`acrux-faucet:${ip}`).digest('hex')
  const remaining = async (address: string, ip: string) => {
    const since = window()
    const [forKey, forIp] = await Promise.all([
      db.faucetGrant.count({ where: { address, grantedAt: { gte: since } } }),
      db.faucetGrant.count({ where: { ipHash: ipHash(ip), grantedAt: { gte: since } } }),
    ])
    return { key: Math.max(0, env.FAUCET_PER_KEY_LIMIT! - forKey), ip: Math.max(0, env.FAUCET_PER_IP_LIMIT! - forIp) }
  }
  const policy = () => ({ amountLamports: env.FAUCET_GRANT_LAMPORTS!, perKey: env.FAUCET_PER_KEY_LIMIT!, perIp: env.FAUCET_PER_IP_LIMIT!, periodSeconds: env.FAUCET_WINDOW_SECONDS!, missingConfiguration: [] })
  app.get('/v1/faucet/:address', async c => {
    const address = readParam(c, 'address', Address), left = await remaining(address, clientIp(c))
    const oldest = await db.faucetGrant.findFirst({ where: { address, grantedAt: { gte: window() } }, orderBy: { grantedAt: 'asc' } })
    return c.json({ address, policy: policy(), remainingForKey: left.key, remainingForIp: left.ip,
      resetsAt: oldest ? new Date(oldest.grantedAt.getTime() + env.FAUCET_WINDOW_SECONDS! * 1000).toISOString() : null, eligible: left.key > 0 && left.ip > 0 })
  })
  app.post('/v1/faucet', async c => {
    const { address } = await readJson(c, FaucetRequest), ip = clientIp(c), left = await remaining(address, ip)
    if (!left.key || !left.ip) fail('FAUCET_LIMIT', 'The faucet limit for this key or IP has been reached.', { retryable: true, retryAfterSeconds: env.FAUCET_WINDOW_SECONDS, stage: 'send' })
    let response: Response
    try { response = await fetch(new URL('/internal/faucet', env.GATEWAY_INTERNAL_URL), { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ address, amountLamports: env.FAUCET_GRANT_LAMPORTS }) }) }
    catch { fail('NET_UNAVAILABLE', 'Acrux Net is unavailable.', { retryable: true, stage: 'send' }) }
    if (!response.ok) fail('NET_UNAVAILABLE', 'Acrux Net could not submit this grant.', { retryable: true, stage: 'send' })
    const grant = z.strictObject({ transactionId: TxId, signature: Signature, balance: Balance }).parse(await response.json())
    const row = await db.faucetGrant.create({ data: { address, ipHash: ipHash(ip), amountLamports: env.FAUCET_GRANT_LAMPORTS!, transactionId: grant.transactionId } })
    return c.json({ id: row.id, transactionId: grant.transactionId, address, amountLamports: env.FAUCET_GRANT_LAMPORTS!,
      signature: grant.signature, balance: grant.balance, grantedAt: row.grantedAt.toISOString() })
  })

  app.get('/v1/lab/scenarios', c => c.json(scenarios))
  app.get('/v1/lab/runs', async c => {
    const rows = await db.labRun.findMany({ orderBy: { createdAt: 'desc' }, take: 200 })
    return c.json(paginate(rows.map(row => lab.view(row)), readQuery(c, PageQuery)))
  })
  // Each run is a sandbox of twelve validators: a few per address an hour, and a short queue.
  const labWindows = createWindows(3_600_000)
  app.post('/v1/lab/runs', async c => {
    const input = await readJson(c, LabRunRequest)
    const retryAfterSeconds = labWindows(clientIp(c), 'lab', env.LAB_RUNS_PER_IP_HOUR)
    if (retryAfterSeconds !== null) fail('RATE_LIMITED', 'This address has started its lab runs for the hour.', { retryable: true, retryAfterSeconds, stage: 'validation' })
    if (lab.pending >= env.LAB_QUEUE_MAX) fail('RATE_LIMITED', 'The Fault Lab queue is full; try again in a minute.', { retryable: true, retryAfterSeconds: 60, stage: 'validation' })
    return c.json(await lab.start(input.scenario, input.seed ?? lab.scenario(input.scenario).seed), 201)
  })
  const readRun = async (id: string) => {
    const row = await db.labRun.findUnique({ where: { id } })
    if (!row) fail('NOT_FOUND', 'This lab run was not found.')
    return row
  }
  app.get('/v1/lab/runs/:id', async c => c.json(lab.view(await readRun(readParam(c, 'id', runId)))))
  app.get('/v1/lab/runs/:id/events', async c => {
    const id = readParam(c, 'id', runId), row = await readRun(id)
    return c.json({ run: lab.view(row), events: await lab.events(id) })
  })
  app.get('/v1/lab/runs/:id/stream', async c => {
    const id = readParam(c, 'id', runId)
    await readRun(id)
    c.header('X-Accel-Buffering', 'no')
    return streamSSE(c, async stream => {
      const queue: ProtocolEvent[] = []
      let wake: (() => void) | undefined, stopped = false
      const stop = lab.listen(id, event => { queue.push(event); wake?.() })
      stream.onAbort(() => { stopped = true; wake?.() })
      const send = (event: ProtocolEvent) => stream.writeSSE({ id: String(event.sequence), event: event.type, data: JSON.stringify(event) })
      try {
        const last = Number(c.req.header('Last-Event-ID') ?? -1)
        let sent = last
        for (const event of await lab.events(id)) if (event.sequence > sent) { await send(event); sent = event.sequence }
        const finished = async () => ['done', 'failed_to_start'].includes((await readRun(id)).status)
        while (!stopped && !(queue.length === 0 && await finished())) {
          const event = queue.shift()
          if (event) { if (event.sequence > sent) { await send(event); sent = event.sequence } continue }
          await new Promise<void>(resolve => { wake = resolve; setTimeout(resolve, 1000) })
        }
      } finally { stop() }
    })
  })
  return { app, close: async () => { registry.stop(); await database.close() } }
}
