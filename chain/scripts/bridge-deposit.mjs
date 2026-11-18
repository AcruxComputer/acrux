// Deposits into acrux_bridge on a local Solana validator for an Acrux recipient: a fresh test mint
// (or --mint), the bridge initialized if it is not yet, and a deposit, printing its record.
// An absent Bridge is initialized from ACRUX_BRIDGE or chain/bridge.json (epochSlots defaults to 432,000).
// Usage: node scripts/bridge-deposit.mjs --recipient <acrux address> [--amount 1000000000] [--rpc http://127.0.0.1:4990]
import { readFileSync } from 'node:fs'
import { isAbsolute, join, resolve } from 'node:path'
import anchor from '@coral-xyz/anchor'
import { TOKEN_PROGRAM_ID, createMint, getOrCreateAssociatedTokenAccount, mintTo } from '@solana/spl-token'
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { chainDir, devKeypair } from './dev-keys.mjs'
import { DEFAULT_EPOCH_SLOTS, chainTag, programData } from './bridge-relay.mjs'

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`)
  return i > 0 ? process.argv[i + 1] : fallback
}
const connection = new Connection(arg('rpc', 'http://127.0.0.1:4990'), 'confirmed')
const recipient = new PublicKey(arg('recipient'))
const amount = BigInt(arg('amount', '1000000000'))
const authority = devKeypair('local-authority')
const depositor = Keypair.generate()
await sendAndConfirmTransaction(connection, new Transaction().add(SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: depositor.publicKey, lamports: LAMPORTS_PER_SOL })), [authority])
const idl = JSON.parse(readFileSync(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8'))
const program = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(depositor), { commitment: 'confirmed' }))
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, program.programId)[0]
const bridge = pda(Buffer.from('bridge'))
if (!(await connection.getAccountInfo(bridge))) {
  const rulePath = process.env.ACRUX_BRIDGE || join(chainDir, 'bridge.json')
  const rule = JSON.parse(readFileSync(isAbsolute(rulePath) ? rulePath : resolve(chainDir, rulePath), 'utf8'))
  const epochSlots = rule.epochSlots ?? DEFAULT_EPOCH_SLOTS
  if (!Number.isSafeInteger(epochSlots) || epochSlots <= 0 || epochSlots % 4) throw new Error('Bridge rule epochSlots must be a positive multiple of four.')
  if (!rule.chainId) throw new Error('Bridge rule chainId is required.')
  const admin = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }))
  await admin.methods.initialize(new anchor.BN(epochSlots), new anchor.BN(Math.floor(rule.fromSlot / epochSlots)), [...chainTag(rule.chainId)])
    .accountsStrict({ payer: authority.publicKey, bridge, program: program.programId, programData: programData(program.programId), systemProgram: SystemProgram.programId }).rpc()
}

const mint = arg('mint') ? new PublicKey(arg('mint')) : await createMint(connection, depositor, depositor.publicKey, null, 6)
const allowed = pda(Buffer.from('allowed'), mint.toBuffer())
if (!(await connection.getAccountInfo(allowed))) {
  const admin = new anchor.Program(idl, new anchor.AnchorProvider(connection, new anchor.Wallet(authority), { commitment: 'confirmed' }))
  await admin.methods.allowMint().accountsStrict({ authority: authority.publicKey, bridge, mint, allowed, systemProgram: SystemProgram.programId }).rpc()
}
const source = (await getOrCreateAssociatedTokenAccount(connection, depositor, mint, depositor.publicKey)).address
await mintTo(connection, depositor, mint, source, depositor, amount)
const sequence = (await program.account.bridge.fetch(bridge)).deposits
const record = pda(Buffer.from('deposit'), sequence.toArrayLike(Buffer, 'le', 8))
const signature = await program.methods.deposit(new anchor.BN(amount.toString()), recipient).accounts({ depositor: depositor.publicKey, mint, source, record, allowed, tokenProgram: TOKEN_PROGRAM_ID }).rpc()
const r = await program.account.depositRecord.fetch(record)
console.log(JSON.stringify({ signature, sequence: r.sequence.toNumber(), mint: r.mint.toBase58(), recipient: r.recipient.toBase58(), amount: r.amount.toString(), slot: r.slot.toNumber() }))
