import { fileURLToPath } from 'node:url'
import { z } from 'zod'
import { Address, U64 } from '@acrux/contract'
import { parseEnv, port, origins, httpUrl } from './engines/server/config.js'

const positiveInteger = z.coerce.number().int().positive().optional()
const naturalInteger = z.coerce.number().int().nonnegative().optional()
const amount = U64.refine(value => value !== '0', 'Must be positive.').optional()

export const env = parseEnv(z.object({
  MODE: z.enum(['fixture', 'node']).default('fixture'),
  /** 127.0.0.1 locally; the container sets 0.0.0.0. */
  HOST: z.enum(['127.0.0.1', '0.0.0.0']).default('127.0.0.1'),
  PORT: port(4920),
  DATABASE_URL: z.string().optional(),
  ACRUX_MINT: z.union([Address, z.literal('')]).default(''),
  LAUNCH_WALLET: Address.default('G9Hx9dH8tNCKdL1FogM2Qsy493Hp6SrG6re1hxkq5aD6'),
  CORS_ORIGINS: origins.prefault('http://127.0.0.1:4910,http://localhost:4910,http://127.0.0.1:4911,http://localhost:4911'),
  GATEWAY_RPC_URL: httpUrl.default('http://127.0.0.1:4930'),
  /** The node's private port (metrics and the faucet's grant route); never public. */
  GATEWAY_INTERNAL_URL: httpUrl.default('http://127.0.0.1:4989'),
  /** The Solana cluster the Hub Registry lives on (the local validator in development). */
  SOLANA_RPC_URL: httpUrl.default('http://127.0.0.1:4990'),
  /** The bridge program (acrux_bridge), on Solana and on Acrux; unset, /v1/bridge waits. */
  BRIDGE_PROGRAM_ID: z.union([Address, z.literal('')]).default(''),
  /** The first Acrux slot of the bridge rule; empty when there is none. */
  BRIDGE_FROM_SLOT: z.union([z.literal(''), z.coerce.number().int().nonnegative()]).default(''),
  /** Bridge reads: 60 s keeps them near five calls a minute on SOLANA_RPC_URL. */
  BRIDGE_POLL_SECONDS: z.coerce.number().positive().default(60),
  /** Requests a second the site's Solana relay may spend on SOLANA_RPC_URL (its share of the shared key). */
  SOLANA_RELAY_RPS: z.coerce.number().positive().default(10),
  /** The node binary: Algorithm 3 for the schedule preview and the Fault Lab runner. */
  ACRUX_BIN: z.string().default(fileURLToPath(new URL('../../node/target/live/acrux.exe', import.meta.url))),
  ACRUX_NODE_DIR: z.string().default(fileURLToPath(new URL('../../node', import.meta.url))),
  ACRUX_GENESIS: z.string().default('../chain/genesis/acrux-net.json'),
  LAB_DIR: z.string().default(fileURLToPath(new URL('../../node/target/lab-runs', import.meta.url))),
  /** Registry poll interval: 2 s against the local validator; 30 s on mainnet keeps the waiting
   * registry under four calls a minute on the shared Helius key. */
  REGISTRY_POLL_SECONDS: z.coerce.number().positive().default(2),
  /** The cluster of the Solana Hub Registry: the local validator here, mainnet-beta on the host. */
  REGISTRY_CLUSTER: z.enum(['localnet', 'mainnet-beta']).default('localnet'),
  /** Lab runs one address may start an hour, and the most runs waiting or running at once. */
  LAB_RUNS_PER_IP_HOUR: z.coerce.number().int().positive().default(6),
  LAB_QUEUE_MAX: z.coerce.number().int().positive().default(10),
  /** The first of the twelve sandbox QUIC ports (5016-5027). */
  LAB_BASE_PORT: port(5016),
  REGISTRY_PROGRAM_ID: Address.optional(),
  REGISTRY_DEPOSIT_LAMPORTS: amount,
  REGISTRY_REGISTRATION_FEE_BPS: z.coerce.number().int().min(0).max(10000).optional(),
  REGISTRY_COOLDOWN_EPOCHS: naturalInteger,
  REGISTRY_INACTIVITY_EPOCHS: positiveInteger,
  REGISTRY_CODE_PATTERN: z.string().optional(),
  REGISTRY_CODE_MIN_LENGTH: positiveInteger,
  REGISTRY_CODE_MAX_LENGTH: positiveInteger,
  REGISTRY_MIN_MEMBERS: z.coerce.number().int().min(3).optional(),
  REGISTRY_MIN_STAKE: amount,
  FAUCET_GRANT_LAMPORTS: amount,
  FAUCET_PER_KEY_LIMIT: positiveInteger,
  FAUCET_PER_IP_LIMIT: positiveInteger,
  FAUCET_WINDOW_SECONDS: positiveInteger,
}))

export const unresolved = (names: readonly (keyof typeof env)[]) => names.filter(name => env[name] === undefined)
export const registrySettings = ['REGISTRY_PROGRAM_ID', 'REGISTRY_DEPOSIT_LAMPORTS', 'REGISTRY_REGISTRATION_FEE_BPS', 'REGISTRY_COOLDOWN_EPOCHS', 'REGISTRY_INACTIVITY_EPOCHS', 'REGISTRY_CODE_PATTERN', 'REGISTRY_CODE_MIN_LENGTH', 'REGISTRY_CODE_MAX_LENGTH', 'REGISTRY_MIN_MEMBERS', 'REGISTRY_MIN_STAKE'] as const
export const faucetSettings = ['FAUCET_GRANT_LAMPORTS', 'FAUCET_PER_KEY_LIMIT', 'FAUCET_PER_IP_LIMIT', 'FAUCET_WINDOW_SECONDS'] as const
