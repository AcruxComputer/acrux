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
import { RegistryIndexer, acruxStake, hubView, memberView, regionOf, solanaStake, type Chain } from './registry.js'
import { schedulePreview } from './schedule.js'
import { readBuyback } from './buyback/read.js'
import { createRpc } from './engines/rpc/client.js'
import { rpcRelay } from './engines/rpc/relay.js'

const chainQuery = z.strictObject({ chain: z.enum(['acrux', 'solana']).optional() })
const epochQuery = z.strictObject({ epoch: z.coerce.number().int().nonnegative().optional(), chain: z.enum(['acrux', 'solana']).optional() })
const runId = z.string().uuid()
// What /hubs/register calls on Solana: read the registry and the hub, build, send, and settle by polling.
const SOLANA_RELAY_METHODS = ['getAccountInfo', 'getMultipleAccounts', 'getBalance', 'getEpochInfo', 'getLatestBlockhash', 'getBlockHeight', 'isBlockhashValid',
  'getSignatureStatuses', 'getMinimumBalanceForRentExemption', 'getFeeForMessage', 'simulateTransaction', 'sendTransaction'] as const
const specPath = fileURLToPath(new URL('../../spec/map.json', import.meta.url))

export function createNodeApi() {
  const database = connectDatabase(env.DATABASE_URL!)
  const db = database.db
  const gateway = createGatewayClient({ rpcUrl: env.GATEWAY_RPC_URL, wsUrl: env.GATEWAY_RPC_URL.replace(/^http/, 'ws').replace(/:(\d+)/, (_, p) => `:${Number(p) + 1}`) })
  // The Hub Registry on both chains: Acrux's own, read through its Solana-compatible RPC every
  // two seconds, and Solana mainnet's, within the shared RPC budget.
  const registries: Record<Chain, RegistryIndexer> = {
    acrux: new RegistryIndexer(db, 'acrux', env.GATEWAY_RPC_URL, env.REGISTRY_PROGRAM_ID!, 2000, acruxStake(env.GATEWAY_RPC_URL), 'acrux'),
    solana: new RegistryIndexer(db, 'solana', env.SOLANA_RPC_URL, env.REGISTRY_PROGRAM_ID!, env.REGISTRY_POLL_SECONDS * 1000, solanaStake(), env.REGISTRY_CLUSTER),
  }
  const lab = new NodeLab(db)
  registries.acrux.start()
  registries.solana.start()
  const app = createApi({ origins: env.CORS_ORIGINS, contractVersion: CONTRACT_VERSION })

  const net = async <M extends RpcMethod>(method: M, params?: unknown) => {
    try { return await gateway.call(method, params as never) }
    catch (error) {
      if (error instanceof ApiClientError) throw new ApiFailure(error.status, error.body)
      fail('NET_UNAVAILABLE', 'Acrux Net is unavailable.', { retryable: true, stage: 'read' })
    }
  }
  const readyRegistry = (chain: Chain) => {
    if (!registries[chain].state.readAt) fail('PROGRAM_UNAVAILABLE', `The Hub Registry on ${chain === 'acrux' ? 'Acrux' : 'Solana'} has not been read yet.`, { retryable: true, stage: 'read' })
    return registries[chain].configView()
  }
  const hubRows = (chain?: Chain) => db.hub.findMany({ where: chain ? { chain } : {}, include: { members: { orderBy: { stakeLamports: 'desc' } } }, orderBy: [{ chain: 'asc' }, { code: 'asc' }] })
  const ready = () => (['acrux', 'solana'] as const).filter(chain => registries[chain].state.readAt)

  app.get('/health', async c => {
    let status: 'waiting' | 'ready' = 'waiting'
    try {
      const response = await fetch(new URL('/health', env.GATEWAY_RPC_URL), { signal: AbortSignal.timeout(2000) })
      const health = await response.json() as { status?: string }
      if (response.ok && health.status === 'ready') status = 'ready'
    } catch { /* An unreachable node is waiting. */ }
    return c.json({ status, contractVersion: CONTRACT_VERSION, missingConfiguration: (['acrux', 'solana'] as const).filter(chain => !registries[chain].state.readAt).map(chain => `Hub Registry on ${chain}`) })
  })
  app.get('/openapi.json', c => c.json(JSON.parse(readFileSync(fileURLToPath(import.meta.resolve('@acrux/contract/openapi.json')), 'utf8'))))
  // The site's Solana RPC for the mainnet Hub Registry: the keyed URL stays here, and the public
  // endpoints refuse a browser origin.
  const solanaRelay = createRpc({ url: env.SOLANA_RPC_URL, service: 'solana-relay', rps: env.SOLANA_RELAY_RPS, scanIntervalMs: 60_000 })
  app.route('/v1/solana-rpc', rpcRelay({ rpc: solanaRelay, methods: SOLANA_RELAY_METHODS, readsPerMinute: 300, writesPerMinute: 20 }))
  app.get('/v1/buyback', async c => c.json(await readBuyback(db, { mint: env.ACRUX_MINT, wallet: env.LAUNCH_WALLET })))

  // Hubs of both chains, each carrying its chain; `?chain=` narrows to one.
  app.get('/v1/hubs', async c => {
    const page = readQuery(c, PageQuery.extend({ chain: z.enum(['acrux', 'solana']).optional() }))
    const chains = page.chain ? [page.chain] : ready()
    if (!chains.length) readyRegistry('acrux')
    const configs = Object.fromEntries(chains.map(chain => [chain, readyRegistry(chain)]))
    const rows = (await hubRows(page.chain)).filter(row => configs[row.chain])
    return c.json(paginate(rows.map(row => hubView(row, configs[row.chain]!)), page))
  })
  // A code can exist on both chains: `?chain=` picks one, else Acrux's comes first.
  app.get('/v1/hubs/:code', async c => {
    const code = readParam(c, 'code', HubCode), { chain } = readQuery(c, chainQuery)
    const rows = await db.hub.findMany({ where: { code, ...(chain ? { chain } : {}) }, include: { members: { orderBy: { stakeLamports: 'desc' } } }, orderBy: { chain: 'asc' } })
    const row = rows[0]
    if (!row) fail('NOT_FOUND', 'This hub was not found.')
    return c.json(hubView(row, readyRegistry(row.chain as Chain)))
  })
  app.get('/v1/registry/config', c => c.json(registries[readQuery(c, chainQuery).chain ?? 'solana'].configView()))
  app.get('/v1/registry/members/:identity', async c => {
    const identity = readParam(c, 'identity', Address), { chain } = readQuery(c, chainQuery)
    const member = await db.member.findFirst({ where: { identity, ...(chain ? { chain } : {}) }, orderBy: { chain: 'asc' } })
    if (!member) fail('NOT_FOUND', 'This validator has no hub membership.')
    return c.json(memberView(member))
  })
  app.get('/v1/schedule', async c => {
    const query = readQuery(c, epochQuery), chain = query.chain ?? 'solana'
    const config = readyRegistry(chain), epoch = query.epoch ?? config.currentEpoch + 1
    const hubs = (await hubRows(chain)).map(row => ({ code: row.code, region: regionOf(row.latitude, row.longitude),
      members: row.members.filter(m => BigInt(m.proposerStakeLamports.toString()) > 0n).map(m => ({ identity: m.identity, stake: Number(BigInt(m.stakeLamports.toString()) / 1_000n) })) }))
    return c.json(await schedulePreview(epoch, hubs, Number(registries[chain].stake.total() / 1_000n)))
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
  return { app, close: async () => { registries.acrux.stop(); registries.solana.stop(); await database.close() } }
}
