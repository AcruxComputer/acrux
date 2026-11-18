import { Connection } from '@solana/web3.js'
import { Hono } from 'hono'
import { connectDatabase } from './db.js'
import { SessionLease } from './engines/indexer/session-lease.js'
import { createKeeperRuntime } from './engines/keeper/runtime.js'
import { createScheduler } from './engines/keeper/scheduler.js'
import { serve } from './engines/server/serve.js'
import { readBuybackConfig, loadLaunchKeypair } from './buyback/config.js'
import { createBuybackService } from './buyback/service.js'
import { readBridgeRelayerConfig, loadBridgeRelayerKeypair } from './bridge-relayer/config.js'
import { createBridgeRelayer, reason } from './bridge-relayer/service.js'

const log = (entry: Record<string, unknown>) => console.log(JSON.stringify(entry))

async function main() {
  const config = readBuybackConfig()
  const bridgeConfig = process.argv.includes('--once') ? null : readBridgeRelayerConfig()
  const signer = config.mint ? await loadLaunchKeypair(config) : null
  const bridgeSigner = bridgeConfig ? await loadBridgeRelayerKeypair(bridgeConfig) : null
  if (bridgeSigner?.publicKey.equals(config.wallet)) throw new Error('The bridge relayer fee wallet must differ from LAUNCH_WALLET.')
  if (!process.env.DATABASE_URL) throw new Error('DATABASE_URL is required.')
  const database = connectDatabase(process.env.DATABASE_URL)
  const connection = new Connection(config.rpcUrl, 'confirmed')
  const lease = new SessionLease({ connectionString: process.env.DATABASE_URL, name: `acrux:buyback:${config.wallet.toBase58()}` })
  let runtime: ReturnType<typeof createKeeperRuntime>
  const service = signer ? createBuybackService({ config, signer, connection, db: database.db, isLeader: () => lease.held && (runtime?.isLeader() ?? true), log }) : null
  const scheduler = createScheduler([{ name: 'buyback', everyMs: config.intervalMs, run: () => service?.runCycle() ?? Promise.resolve() }], {
    onError: (_task, error) => log({ event: 'buyback_cycle_error', error: error instanceof Error ? error.name : 'Error' }),
  })
  runtime = createKeeperRuntime({
    lease: { acquire: () => lease.acquire(), renew: async () => lease.held, release: () => lease.release() },
    leaseMs: 15_000,
    tickMs: 5_000,
    gate: async () => config.mint ? 'ready' : 'waiting',
    tick: () => scheduler.tick(),
    log: entry => log({ event: entry.event, task: 'buyback', error: 'KeeperError' }),
  })
  // Keep the existing one-off command scoped to the buyback; normal service startup runs both.
  if (process.argv.includes('--once')) {
    try {
      if (!await lease.acquire()) throw new Error('Another buyback keeper holds the lease.')
      await service?.runCycle()
      log({ event: 'buyback_once', status: config.mint ? 'live' : 'waiting', dryRun: config.dryRun })
    } finally {
      await lease.release()
      await database.close()
    }
    return
  }
  const bridgeLease = new SessionLease({ connectionString: process.env.DATABASE_URL, name: `acrux:bridge:${bridgeConfig?.program.toBase58() ?? 'waiting'}` })
  let bridgeRuntime: ReturnType<typeof createKeeperRuntime>
  const relayer = bridgeConfig && bridgeSigner ? createBridgeRelayer({ config: bridgeConfig, signer: bridgeSigner, db: database.db, isLeader: () => bridgeLease.held && (bridgeRuntime?.isLeader() ?? true), log }) : null
  const bridgeScheduler = createScheduler([{ name: 'bridge', everyMs: bridgeConfig?.intervalMs ?? 15_000, run: () => relayer?.runCycle() ?? Promise.resolve() }], {
    onError: (_task, error) => log({ event: 'bridge_cycle_error', error: reason(error) }),
  })
  bridgeRuntime = createKeeperRuntime({
    lease: { acquire: () => bridgeLease.acquire(), renew: async () => bridgeLease.held, release: () => bridgeLease.release() },
    leaseMs: 15_000,
    tickMs: 5_000,
    gate: async () => bridgeConfig ? 'ready' : 'waiting',
    tick: () => bridgeScheduler.tick(),
    log: entry => log({ event: entry.event, task: 'bridge', error: 'KeeperError' }),
  })
  const port = Number(process.env.PORT || 4920)
  if (!Number.isSafeInteger(port) || port < 1 || port > 65535) throw new Error('PORT must be a TCP port.')
  const app = new Hono().get('/health', c => c.json({ ...runtime.health, mint: config.mint?.toBase58() ?? null, wallet: config.wallet.toBase58(), dryRun: config.dryRun,
    bridge: { ...bridgeRuntime.health, program: bridgeConfig?.program.toBase58() ?? null, wallet: bridgeSigner?.publicKey.toBase58() ?? null } }))
  const serving = serve(app, { host: process.env.HOST || '127.0.0.1', port, close: async () => { await Promise.all([runtime.stop(), bridgeRuntime.stop()]); await database.close() } })
  void Promise.all([runtime.run(), bridgeRuntime.run()]).catch(async () => { await serving.shutdown(); process.exitCode = 1 })
  log({ event: 'buyback_started', status: config.mint ? 'live' : 'waiting', wallet: config.wallet.toBase58(), mint: config.mint?.toBase58() ?? null, dryRun: config.dryRun })
  log({ event: 'bridge_relayer_started', status: bridgeConfig ? 'live' : 'waiting', program: bridgeConfig?.program.toBase58() ?? null, wallet: bridgeSigner?.publicKey.toBase58() ?? null })
}

void main().catch(error => {
  log({ event: 'keeper_start_failed', error: error instanceof Error ? error.message : 'Keeper configuration failed.' })
  process.exitCode = 1
})