// Writes genesis/acrux-net.json: the accounts Acrux Net loads at genesis for the demo pool.
// The output is a pure function of the development seed and the constants below.
import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { PublicKey, SystemProgram } from '@solana/web3.js'
import { DEV_SEED, GENESIS_SEED, chainDir, devKeypair } from './dev-keys.mjs'

const TOKEN_PROGRAM = new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA')
const ATA_PROGRAM = new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL')

const DECIMALS = 6
const FEE_BPS = 30
const RESERVE_A = 1_000_000_000_000n // 1,000,000 AXA
const RESERVE_B = 2_000_000_000_000n // 2,000,000 AXB
const FAUCET_TOKENS = 1_000_000_000_000_000n // 1,000,000,000 of each mint
const FAUCET_LAMPORTS = 100_000_000_000 // 100 SOL for the faucet's fees

// Acrux Net's rent, the default of the embedded SVM: 3,480 lamports per byte-year,
// exempt at two years, 128 bytes of account overhead.
const rentExempt = (space) => (128 + space) * 3480 * 2

const idl = JSON.parse(readFileSync(join(chainDir, 'target/idl/demo_pool.json'), 'utf8'))
const program = new PublicKey(idl.address)
if (!program.equals(devKeypair('demo-pool-program').publicKey)) throw new Error('demo_pool id is not the dev seed id')

const mintA = devKeypair('mint-a').publicKey
const mintB = devKeypair('mint-b').publicKey
const mintAuthority = devKeypair('mint-authority').publicKey
const faucet = devKeypair('faucet').publicKey

const [pool, poolBump] = PublicKey.findProgramAddressSync([Buffer.from('pool'), mintA.toBuffer(), mintB.toBuffer()], program)
const vault = (mint) => PublicKey.findProgramAddressSync([Buffer.from('vault'), pool.toBuffer(), mint.toBuffer()], program)[0]
const ata = (owner, mint) => PublicKey.findProgramAddressSync([owner.toBuffer(), TOKEN_PROGRAM.toBuffer(), mint.toBuffer()], ATA_PROGRAM)[0]
const vaultA = vault(mintA)
const vaultB = vault(mintB)
const faucetA = ata(faucet, mintA)
const faucetB = ata(faucet, mintB)

function mintData(supply) {
  const data = Buffer.alloc(82)
  data.writeUInt32LE(1, 0) // mint authority: some
  mintAuthority.toBuffer().copy(data, 4)
  data.writeBigUInt64LE(supply, 36)
  data[44] = DECIMALS
  data[45] = 1 // initialized; no freeze authority
  return data
}

function tokenData(mint, owner, amount) {
  const data = Buffer.alloc(165)
  mint.toBuffer().copy(data, 0)
  owner.toBuffer().copy(data, 32)
  data.writeBigUInt64LE(amount, 64)
  data[108] = 1 // initialized; no delegate, not native, no close authority
  return data
}

const poolData = Buffer.concat([
  Buffer.from(idl.accounts.find((a) => a.name === 'Pool').discriminator),
  mintA.toBuffer(), mintB.toBuffer(), vaultA.toBuffer(), vaultB.toBuffer(),
  Buffer.from([FEE_BPS & 0xff, FEE_BPS >> 8, poolBump]),
])

const account = (label, address, owner, data, lamports = rentExempt(data.length)) => ({
  label, address: address.toBase58(), owner: owner.toBase58(), lamports, data: data.toString('base64'), executable: false,
})

const genesis = {
  description: GENESIS_SEED === DEV_SEED
    ? 'Acrux Net genesis state for the demo pool. Development keys, local test funds only.'
    : 'Acrux Net genesis state for the demo pool, from a secret seed. Keep this file on its host.',
  seed: GENESIS_SEED,
  derivation: 'keypair = ed25519 keypair from seed sha256(seed + "/" + label)',
  programs: [{ name: 'demo_pool', address: program.toBase58(), path: 'target/deploy/demo_pool.so' }],
  tokens: [
    { symbol: 'AXA', name: 'Acrux Demo A', decimals: DECIMALS, mint: mintA.toBase58() },
    { symbol: 'AXB', name: 'Acrux Demo B', decimals: DECIMALS, mint: mintB.toBase58() },
  ],
  pool: {
    address: pool.toBase58(), mintA: mintA.toBase58(), mintB: mintB.toBase58(),
    vaultA: vaultA.toBase58(), vaultB: vaultB.toBase58(), feeBps: FEE_BPS,
    reserves: [RESERVE_A.toString(), RESERVE_B.toString()],
  },
  mintAuthority: mintAuthority.toBase58(),
  faucet: {
    address: faucet.toBase58(), lamports: FAUCET_LAMPORTS,
    tokenAccounts: [faucetA.toBase58(), faucetB.toBase58()], tokens: FAUCET_TOKENS.toString(),
  },
  accounts: [
    account('mint AXA', mintA, TOKEN_PROGRAM, mintData(RESERVE_A + FAUCET_TOKENS)),
    account('mint AXB', mintB, TOKEN_PROGRAM, mintData(RESERVE_B + FAUCET_TOKENS)),
    account('pool', pool, program, poolData),
    account('pool vault AXA', vaultA, TOKEN_PROGRAM, tokenData(mintA, pool, RESERVE_A)),
    account('pool vault AXB', vaultB, TOKEN_PROGRAM, tokenData(mintB, pool, RESERVE_B)),
    account('faucet', faucet, SystemProgram.programId, Buffer.alloc(0), FAUCET_LAMPORTS),
    account('faucet AXA', faucetA, TOKEN_PROGRAM, tokenData(mintA, faucet, FAUCET_TOKENS)),
    account('faucet AXB', faucetB, TOKEN_PROGRAM, tokenData(mintB, faucet, FAUCET_TOKENS)),
  ],
}

// node scripts/genesis.mjs [out]: a host writes its own, with ACRUX_GENESIS_SEED set.
const out = process.argv[2] ?? join(chainDir, 'genesis/acrux-net.json')
writeFileSync(out, JSON.stringify(genesis, null, 2) + '\n')
console.log(out)
