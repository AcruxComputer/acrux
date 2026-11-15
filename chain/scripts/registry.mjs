// hub_registry client pieces shared by the local setup and the tests.
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import anchor from '@coral-xyz/anchor'
import { Connection, PublicKey, SystemProgram } from '@solana/web3.js'
import { chainDir, devKeypair } from './dev-keys.mjs'

export const RPC_URL = 'http://127.0.0.1:4990'
export const idl = JSON.parse(readFileSync(join(chainDir, 'target/idl/hub_registry.json'), 'utf8'))
export const programId = new PublicKey(idl.address)

const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds.map((s) => (typeof s === 'string' ? Buffer.from(s) : s)), programId)[0]
export const registryAddress = pda('registry')
export const feeVaultAddress = pda('fee_vault')
export const hubAddress = (code) => pda('hub', code)
export const membershipAddress = (identity) => pda('member', identity.toBuffer())
export const eventAuthority = pda('__event_authority')

/** The configuration the local validator's registry is initialized with. */
export const LOCAL_CONFIG = {
  feeDestination: devKeypair('local-fee-destination').publicKey,
  depositLamports: new anchor.BN(1_000_000_000), // 1 SOL
  registrationFeeBps: 500, // 5% of the deposit
  cooldownEpochs: new anchor.BN(1),
  inactivityEpochs: new anchor.BN(4),
  minMembers: 4, // n_min
  codeMinLen: 3,
  codeMaxLen: 8,
}

export function registryProgram(connection = new Connection(RPC_URL, 'confirmed'), payer = devKeypair('local-authority')) {
  const provider = new anchor.AnchorProvider(connection, new anchor.Wallet(payer), { commitment: 'confirmed' })
  return new anchor.Program(idl, provider)
}

/** Runs `initialize` as the upgrade authority unless the registry already exists. */
export async function initializeLocal(program = registryProgram()) {
  const existing = await program.account.registry.fetchNullable(registryAddress)
  if (existing) return existing
  const loader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111')
  await program.methods.initialize(LOCAL_CONFIG).accountsStrict({
    authority: program.provider.publicKey,
    registry: registryAddress,
    feeVault: feeVaultAddress,
    program: programId,
    programData: PublicKey.findProgramAddressSync([programId.toBuffer()], loader)[0],
    systemProgram: SystemProgram.programId,
  }).rpc()
  return program.account.registry.fetch(registryAddress)
}
