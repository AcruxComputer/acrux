import { readFile, stat } from 'node:fs/promises'
import { Keypair, PublicKey } from '@solana/web3.js'

export type BridgeRelayerConfig = {
  program: PublicKey
  rpcUrl: string
  gatewayUrl: string
  gatewayInternalUrl: string
  keypairFile: string
  intervalMs: number
  maxNewWithdrawalsPerCycle: number
  maxRetryWithdrawalsPerCycle: number
  /** Parked withdrawals waiting for the recipient's token account: a budget apart, so a flood of them never delays other retries. */
  maxAccountRetryWithdrawalsPerCycle: number
  maxSavedCblocksPerCycle: number
}

export function readBridgeRelayerConfig(env: NodeJS.ProcessEnv = process.env): BridgeRelayerConfig | null {
  const rawProgram = env.BRIDGE_PROGRAM_ID?.trim()
  if (!rawProgram) return null
  let program: PublicKey
  try { program = new PublicKey(rawProgram) } catch { throw new Error('BRIDGE_PROGRAM_ID must be a Solana public key.') }
  const url = (name: string) => {
    const value = env[name]?.trim()
    if (!value) throw new Error(`${name} is required when BRIDGE_PROGRAM_ID is set.`)
    try { if (!['http:', 'https:'].includes(new URL(value).protocol)) throw new Error() }
    catch { throw new Error(`${name} must be an HTTP URL.`) }
    return value
  }
  const keypairFile = env.BRIDGE_RELAYER_KEYPAIR_FILE?.trim()
  if (!keypairFile) throw new Error('BRIDGE_RELAYER_KEYPAIR_FILE is required when BRIDGE_PROGRAM_ID is set.')
  const rawInterval = env.BRIDGE_RELAYER_INTERVAL_SECONDS?.trim() || '15'
  const seconds = Number(rawInterval)
  if (!/^\d+$/.test(rawInterval) || !Number.isSafeInteger(seconds) || seconds < 1 || seconds > 60) throw new Error('BRIDGE_RELAYER_INTERVAL_SECONDS must be 1 to 60.')
  const batchSize = (name: string, fallback = 10) => {
    const raw = env[name]?.trim() || String(fallback)
    const value = Number(raw)
    if (!/^\d+$/.test(raw) || !Number.isSafeInteger(value) || value < 1 || value > 2_147_483_647) throw new Error(`${name} must be a positive 32-bit integer.`)
    return value
  }
  return {
    program, rpcUrl: url('SOLANA_RPC_URL'), gatewayUrl: url('GATEWAY_RPC_URL'), gatewayInternalUrl: url('GATEWAY_INTERNAL_URL'), keypairFile, intervalMs: seconds * 1000,
    maxNewWithdrawalsPerCycle: batchSize('BRIDGE_RELAYER_MAX_NEW_WITHDRAWALS_PER_CYCLE'),
    maxRetryWithdrawalsPerCycle: batchSize('BRIDGE_RELAYER_MAX_RETRY_WITHDRAWALS_PER_CYCLE'),
    maxAccountRetryWithdrawalsPerCycle: batchSize('BRIDGE_RELAYER_MAX_ACCOUNT_RETRY_WITHDRAWALS_PER_CYCLE', 30),
    maxSavedCblocksPerCycle: batchSize('BRIDGE_RELAYER_MAX_SAVED_CBLOCKS_PER_CYCLE', 512),
  }
}

export async function loadBridgeRelayerKeypair(config: BridgeRelayerConfig): Promise<Keypair> {
  try {
    const info = await stat(config.keypairFile)
    if (!info.isFile() || (process.platform !== 'win32' && (info.mode & 0o777) !== 0o600)) throw new Error()
    const bytes: unknown = JSON.parse(await readFile(config.keypairFile, 'utf8'))
    if (!Array.isArray(bytes) || bytes.length !== 64 || !bytes.every(value => Number.isInteger(value) && value >= 0 && value <= 255)) throw new Error()
    return Keypair.fromSecretKey(Uint8Array.from(bytes))
  } catch { throw new Error('BRIDGE_RELAYER_KEYPAIR_FILE must contain a valid 64-byte keypair in a regular mode-600 file.') }
}