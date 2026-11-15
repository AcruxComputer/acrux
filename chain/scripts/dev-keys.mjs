// Development keys for Acrux Net genesis and the local validator.
// Every key here derives from a published seed and only ever holds local test funds.
import { createHash } from 'node:crypto'
import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { Keypair } from '@solana/web3.js'

export const DEV_SEED = 'acrux-dev-seed-v1'
/** A public net's genesis keys (faucet, mints, mint authority) come from a secret seed instead. */
export const GENESIS_SEED = process.env.ACRUX_GENESIS_SEED || DEV_SEED
const SEEDED = new Set(['mint-a', 'mint-b', 'mint-authority', 'faucet'])

export const LABELS = [
  'demo-pool-program', // program id of the demo pool on Acrux Net
  'mint-a', // AXA
  'mint-b', // AXB
  'mint-authority', // may mint more AXA and AXB
  'faucet', // owns the genesis token accounts the faucet pays from
  'local-authority', // local validator: upgrade authority, registry authority, genesis lamports
  'local-fee-destination', // local validator: where sweep_fees sends the fee vault
]

/** keypair = ed25519 from sha256(DEV_SEED + "/" + label) */
export function devKeypair(label) {
  if (!LABELS.includes(label)) throw new Error(`Unknown dev key label: ${label}`)
  const seed = SEEDED.has(label) ? GENESIS_SEED : DEV_SEED
  return Keypair.fromSeed(createHash('sha256').update(`${seed}/${label}`).digest())
}

export const chainDir = join(dirname(fileURLToPath(import.meta.url)), '..')

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const dir = join(chainDir, 'keys', 'dev')
  mkdirSync(dir, { recursive: true })
  for (const label of LABELS) {
    const keypair = devKeypair(label)
    writeFileSync(join(dir, `${label}.json`), JSON.stringify(Array.from(keypair.secretKey)))
    console.log(`${label.padEnd(18)} ${keypair.publicKey.toBase58()}`)
  }
}
