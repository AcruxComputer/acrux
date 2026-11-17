import { Connection } from '@solana/web3.js'
import { Hono } from 'hono'
import { connectDatabase } from './db.js'
import { SessionLease } from './engines/indexer/session-lease.js'
import { createKeeperRuntime } from './engines/keeper/runtime.js'
import { createScheduler } from './engines/keeper/scheduler.js'
import { serve } from './engines/server/serve.js'
import { readBuybackConfig, loadLaunchKeypair } from './buyback/config.js'
import { createBuybackService } from './buyback/service.js'

const log = (entry: Record<string, unknown>) => console.log(JSON.stringify(entry))

async function main() {
  const config = readBuybackConfig()
  const signer = await loadLaunchKeypair(config)
  if (!process.env.DATABASE_URL) throw new Error('DATABASE_URL is required.')
  const database = connectDatabase(process.env.DATABASE_URL)
  const connection = new Connection(config.rpcUrl, 'confirmed')
  const lease = new SessionLease({ connectionString: process.env.DATABASE_URL, name: `acrux:buyback:${config.wallet.toBase58()}` })
  let runtime: ReturnType<typeof createKeeperRuntime>
  const service = createBuybackService({ config, signer, connection, db: database.db, isLeader: () => lease.held && (runtime?.isLeader() ?? true), log })
  const scheduler = createScheduler([{ name: 'buyback', everyMs: config.intervalMs, run: service.runCycle }], {
    onError: (_task, error) => log({ event: 'buyback_cycle_error', error: error instanceof Error ? error.name : 'Error' }),
  })
  runtime = createKeeperRuntime({
    lease: { acquire: () => lease.acquire(), renew: async () => lease.held, release: () => lease.release() },
    leaseMs: 15_000,
    tickMs: 5_000,
    gate: async () => config.mint ? 'ready' : 'waiting',
    tick: () => scheduler.tick(),
    log: entry => log({ event: entry.event, error: 'KeeperError' }),
  })
  if (process.argv.includes('--once')) {
    try {
      if (!await lease.acquire()) throw new Error('Another buyback keeper holds the lease.')
      // A single dry run is useful for inspecting a mainnet quote without sending it.
      const once = createBuybackService({ config, signer, connection, db: database.db, isLeader: () => lease.held, log })
      await once.runCycle()
      log({ event: 'buyback_once', status: config.mint ? 'live' : 'waiting', dryRun: config.dryRun })
    } finally {
      await lease.release()
      await database.close()
    }
    return
  }
  const port = Number(process.env.PORT || 4920)
  if (!Number.isSafeInteger(port) || port < 1 || port > 65535) throw new Error('PORT must be a TCP port.')
  const app = new Hono().get('/health', c => c.json({ ...runtime.health, mint: config.mint?.toBase58() ?? null, wallet: config.wallet.toBase58(), dryRun: config.dryRun }))
  const serving = serve(app, { host: process.env.HOST || '127.0.0.1', port, close: async () => { await runtime.stop(); await database.close() } })
  void runtime.run().catch(async () => { await serving.shutdown(); process.exitCode = 1 })
  log({ event: 'buyback_started', status: config.mint ? 'live' : 'waiting', wallet: config.wallet.toBase58(), mint: config.mint?.toBase58() ?? null, dryRun: config.dryRun })
}

void main().catch(error => {
  log({ event: 'buyback_start_failed', error: error instanceof Error ? error.name : 'Error' })
  process.exitCode = 1
})
