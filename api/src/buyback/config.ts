import { readFile, stat } from 'node:fs/promises'
import { Keypair, PublicKey } from '@solana/web3.js'

export const LAUNCH_WALLET = 'G9Hx9dH8tNCKdL1FogM2Qsy493Hp6SrG6re1hxkq5aD6'
const U64_MAX = 18_446_744_073_709_551_615n

export type BuybackConfig = {
  mint: PublicKey | null
  wallet: PublicKey
  launchpad: 'launchlab' | 'pumpfun'
  intervalMs: number
  solFloorLamports: bigint
  minSwapLamports: bigint
  slippageBps: number
  postGraduationBps: number
  dryRun: boolean
  rpcUrl: string
  keypairFile: string
  jupiterApiKey?: string
}

export function readBuybackConfig(env: NodeJS.ProcessEnv = process.env): BuybackConfig {
  const amount = (name: string, fallback: string) => {
    const raw = env[name]?.trim() || fallback
    if (!/^(0|[1-9]\d*)$/.test(raw) || BigInt(raw) > U64_MAX) throw new Error(`${name} must be a u64 decimal string.`)
    return BigInt(raw)
  }
  const integer = (name: string, fallback: number, min: number, max: number) => {
    const raw = env[name]?.trim() || String(fallback)
    const value = Number(raw)
    if (!/^\d+$/.test(raw) || !Number.isSafeInteger(value) || value < min || value > max) throw new Error(`${name} is outside its allowed range.`)
    return value
  }
  const address = (name: string, raw: string) => {
    try { return new PublicKey(raw) } catch { throw new Error(`${name} must be a Solana public key.`) }
  }
  const launchpad = env.BUYBACK_LAUNCHPAD?.trim() || 'launchlab'
  if (launchpad !== 'launchlab' && launchpad !== 'pumpfun') throw new Error('BUYBACK_LAUNCHPAD is not implemented.')
  const dryRun = env.BUYBACK_DRY_RUN?.trim() || 'false'
  if (dryRun !== 'true' && dryRun !== 'false') throw new Error('BUYBACK_DRY_RUN must be true or false.')
  const rpcUrl = env.SOLANA_RPC_URL?.trim() || 'http://127.0.0.1:4990'
  try {
    if (!['http:', 'https:'].includes(new URL(rpcUrl).protocol)) throw new Error()
  } catch { throw new Error('SOLANA_RPC_URL must be an HTTP URL.') }
  return {
    mint: env.ACRUX_MINT?.trim() ? address('ACRUX_MINT', env.ACRUX_MINT.trim()) : null,
    wallet: address('LAUNCH_WALLET', env.LAUNCH_WALLET?.trim() || LAUNCH_WALLET),
    launchpad,
    intervalMs: integer('BUYBACK_INTERVAL_SECONDS', 600, 1, 2_147_483) * 1000,
    solFloorLamports: amount('BUYBACK_SOL_FLOOR_LAMPORTS', '20000000'),
    minSwapLamports: amount('BUYBACK_MIN_SWAP_LAMPORTS', '50000000'),
    slippageBps: integer('BUYBACK_SLIPPAGE_BPS', 300, 0, 10_000),
    postGraduationBps: integer('BUYBACK_POST_GRADUATION_BPS', 2500, 0, 10_000),
    dryRun: dryRun === 'true',
    rpcUrl,
    keypairFile: env.LAUNCH_KEYPAIR_FILE?.trim() || '/etc/acrux/launch-keypair.json',
    jupiterApiKey: env.JUPITER_API_KEY?.trim() || undefined,
  }
}

export async function loadLaunchKeypair(config: BuybackConfig): Promise<Keypair> {
  let keypair: Keypair
  try {
    const info = await stat(config.keypairFile)
    if (!info.isFile() || (process.platform !== 'win32' && (info.mode & 0o777) !== 0o600)) throw new Error()
    const raw: unknown = JSON.parse(await readFile(config.keypairFile, 'utf8'))
    if (!Array.isArray(raw) || raw.length !== 64 || !raw.every(value => Number.isInteger(value) && value >= 0 && value <= 255)) throw new Error()
    keypair = Keypair.fromSecretKey(Uint8Array.from(raw))
  } catch { throw new Error('LAUNCH_KEYPAIR_FILE must contain a valid 64-byte keypair in a regular mode-600 file.') }
  if (!keypair.publicKey.equals(config.wallet)) throw new Error('LAUNCH_KEYPAIR_FILE does not match LAUNCH_WALLET.')
  return keypair
}
