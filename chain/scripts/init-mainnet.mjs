// Initializes the Hub Registry on mainnet once the program is deployed. Idempotent: an existing
// registry is printed and left alone. Reads only mainnet names; never the local .env.
//   RPC_URL               a mainnet RPC URL
//   AUTHORITY_KEYPAIR     the program's upgrade authority, which alone may initialize
//   FEE_DESTINATION       where sweep_fees sends the registry's fees
//   REGISTRATION_FEE_BPS  the registration fee, basis points of the deposit (the owner's rate)
//   DEPOSIT_LAMPORTS, COOLDOWN_EPOCHS, INACTIVITY_EPOCHS, MIN_MEMBERS (defaults: 1 SOL, 1, 4, 4)
import { readFileSync } from 'node:fs'
import anchor from '@coral-xyz/anchor'
import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js'
import { feeVaultAddress, idl, programId, registryAddress } from './registry.mjs'

const need = (name) => { const v = process.env[name]; if (!v) throw new Error(`Set ${name}`); return v }
const connection = new Connection(need('RPC_URL'), 'confirmed')
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(need('AUTHORITY_KEYPAIR'), 'utf8'))))
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }))

const existing = await program.account.registry.fetchNullable(registryAddress)
if (existing) {
  console.log('Registry already initialized', registryAddress.toBase58(), JSON.stringify(existing))
  process.exit(0)
}
const config = {
  feeDestination: new PublicKey(need('FEE_DESTINATION')),
  depositLamports: new anchor.BN(process.env.DEPOSIT_LAMPORTS ?? '1000000000'),
  registrationFeeBps: Number(need('REGISTRATION_FEE_BPS')),
  cooldownEpochs: new anchor.BN(process.env.COOLDOWN_EPOCHS ?? '1'),
  inactivityEpochs: new anchor.BN(process.env.INACTIVITY_EPOCHS ?? '4'),
  minMembers: Number(process.env.MIN_MEMBERS ?? '4'),
  codeMinLen: 3,
  codeMaxLen: 8,
}
const loader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111')
const signature = await program.methods.initialize(config).accountsStrict({
  authority: authority.publicKey, registry: registryAddress, feeVault: feeVaultAddress, program: programId,
  programData: PublicKey.findProgramAddressSync([programId.toBuffer()], loader)[0], systemProgram: SystemProgram.programId,
}).rpc()
console.log('initialize', signature)
console.log('Registry', registryAddress.toBase58(), 'fee vault', feeVaultAddress.toBase58())
