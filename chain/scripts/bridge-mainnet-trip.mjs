// Road build 3 step 6, the first real round trip: a holder deposits ACRUX on Solana mainnet to its
// own address on Acrux, waits for the credit, then withdraws the same amount back to its own
// Solana token account (which exists, since the deposit came from it) and waits for the release.
// Prints each signature and what /v1/bridge reads at each step. The holder needs the ACRUX and a
// little SOL on Solana; its Acrux fee comes from the faucet when it holds none.
// Usage (from backend/chain):
//   node scripts/bridge-mainnet-trip.mjs --rpc <solana rpc url> --keypair <holder keypair file> \
//     [--acrux https://rpc.acrux.computer] [--api https://api.acrux.computer] \
//     [--mint 7Evw4uxTxLGGEFEvXLUNYyc1J7PnFk9EYwTUFagARUX] [--amount 1000000]
import { readFileSync } from 'node:fs'
import anchor from '@coral-xyz/anchor'
import { TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync, unpackAccount } from '@solana/spl-token'
import { Connection, Keypair, PublicKey, SystemProgram, Transaction } from '@solana/web3.js'
import { bridgeProgram, pdas } from './bridge-relay.mjs'

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`)
  if (i < 0) { if (fallback === undefined) throw new Error(`--${name} is required`); return fallback }
  return process.argv[i + 1]
}
const holder = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(arg('keypair'), 'utf8'))))
const api = arg('api', 'https://api.acrux.computer')
const mint = new PublicKey(arg('mint', '7Evw4uxTxLGGEFEvXLUNYyc1J7PnFk9EYwTUFagARUX'))
const amount = BigInt(arg('amount', '1000000'))
const solana = new Connection(arg('rpc'), 'confirmed'), acrux = new Connection(arg('acrux', 'https://rpc.acrux.computer'), 'confirmed')
const onSolana = bridgeProgram(solana, holder), onAcrux = bridgeProgram(acrux, holder)
const programId = onSolana.programId
const pda = (...seeds) => PublicKey.findProgramAddressSync(seeds, programId)[0]
const u64 = n => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n.toString())); return b }
const { bridge, allowed } = pdas(programId)
const bridgedMint = pda(Buffer.from('mint'), mint.toBuffer())
const source = getAssociatedTokenAddressSync(mint, holder.publicKey, false, TOKEN_PROGRAM_ID)
const acruxSource = getAssociatedTokenAddressSync(bridgedMint, holder.publicKey, false, TOKEN_PROGRAM_ID)
const pause = ms => new Promise(r => setTimeout(r, ms))
const log = (event, fields) => console.log(JSON.stringify({ event, at: new Date().toISOString(), ...fields }))
const json = async (path, body) => (await fetch(`${api}${path}`, body ? { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) } : undefined)).json()
const balance = async (connection, address) => {
  const info = await connection.getAccountInfo(address, 'confirmed')
  return info ? unpackAccount(address, info, TOKEN_PROGRAM_ID).amount : 0n
}
async function until(label, seconds, read) {
  for (const deadline = Date.now() + seconds * 1000; Date.now() < deadline; await pause(3000)) {
    const value = await read()
    if (value) return value
  }
  throw new Error(`${label}: not seen within ${seconds} s`)
}
const transfer = async (kind, sequence) => (await json(`/v1/bridge/transfers?kind=${kind}&address=${holder.publicKey.toBase58()}`)).items.find(row => row.sequence === sequence)
const vaults = async () => (await json('/v1/bridge')).vaults.map(v => ({ solanaMint: v.solanaMint, locked: v.lockedBaseUnits, bridged: v.bridgedBaseUnits }))

log('trip_started', { holder: holder.publicKey.toBase58(), program: programId.toBase58(), amount: amount.toString(), solanaAcrux: (await balance(solana, source)).toString() })

// 1. Deposit on Solana to the same key on Acrux.
const sequence = (await onSolana.account.bridge.fetch(bridge)).deposits
const deposit = await onSolana.methods.deposit(new anchor.BN(amount.toString()), holder.publicKey).accountsStrict({
  depositor: holder.publicKey, bridge, mint, allowed: allowed(mint), source,
  vault: pda(Buffer.from('vault'), mint.toBuffer()), record: pda(Buffer.from('deposit'), u64(sequence)),
  tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  eventAuthority: pda(Buffer.from('__event_authority')), program: programId,
}).rpc()
log('deposit_sent', { signature: deposit, sequence: sequence.toString() })
const credited = await until('The credit on Acrux', 600, async () => {
  const row = await transfer('deposit', sequence.toNumber())
  return row?.status === 'credited' && row
})
log('deposit_credited', { acruxBalance: (await balance(acrux, acruxSource)).toString(), transfer: credited, vaults: await vaults() })

// 2. The withdrawal back, burned on Acrux to the holder's own Solana token account.
if (await acrux.getBalance(holder.publicKey, 'confirmed') === 0) {
  log('faucet', await json('/v1/faucet', { address: holder.publicKey.toBase58() }))
}
const outbound = pda(Buffer.from('outbound'))
const next = (await onAcrux.account.outbound.fetchNullable(outbound))?.withdrawals ?? new anchor.BN(0)
const withdraw = await onAcrux.methods.withdraw(mint, new anchor.BN(amount.toString()), holder.publicKey).accountsStrict({
  owner: holder.publicKey, mint: bridgedMint, minimum: pda(Buffer.from('minimum'), mint.toBuffer()), source: acruxSource,
  outbound, record: pda(Buffer.from('withdrawal'), u64(next)), tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
}).instruction()
const burn = await onAcrux.provider.sendAndConfirm(new Transaction().add(withdraw), [])
log('withdrawal_burned', { signature: burn, sequence: next.toString() })
for (const status of ['certified', 'released']) {
  const row = await until(`The withdrawal ${status}`, 900, async () => {
    const found = await transfer('withdrawal', next.toNumber())
    if (found?.releaseError) log('withdrawal_waiting', { releaseError: found.releaseError })
    return (found?.status === status || found?.status === 'released') && found
  })
  log(`withdrawal_${status}`, { transfer: row })
}
log('trip_done', { solanaAcrux: (await balance(solana, source)).toString(), vaults: await vaults() })
