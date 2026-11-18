// Road build 3, step 4: a token's round trip, Solana to Acrux and back, on the local validator.
// Starts the validator with acrux_bridge, and an Acrux net whose bridge rule starts at slot 0
// with the same program placed on Acrux. Seeds the light client, deposits 2,500 tokens for an Acrux
// holder, waits for the credit, withdraws 1,000 on Acrux to a Solana recipient, relays the cblock
// of the burn and checks the payout from the vault. Stops both.
// Usage (from backend/chain): node scripts/bridge-round-trip-check.mjs [acrux binary, from backend/node]
import { spawn, spawnSync } from 'node:child_process'
import { mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import anchor from '@coral-xyz/anchor'
import { TOKEN_PROGRAM_ID, getAccount, getAssociatedTokenAddressSync } from '@solana/spl-token'
import { Connection, Keypair, PublicKey } from '@solana/web3.js'
import { devKeypair } from './dev-keys.mjs'
import { EPOCH_SLOTS, bridgeProgram, cblockProof, relay, seed } from './bridge-relay.mjs'

const chain = resolve(import.meta.dirname, '..')
const node = resolve(chain, '../node')
const acrux = resolve(node, process.argv[2] ?? 'target/release/acrux')
const program = 'EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5'
const solanaUrl = 'http://127.0.0.1:4990'
const gateway = 'http://127.0.0.1:4930'
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const rpc = async (url, method, params) => (await (await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json()).result
const up = async (url) => { for (let i = 0; i < 120; i++) { if ((await rpc(url, 'getSlot', []).catch(() => 0)) > 2) return; await sleep(1000) } throw new Error(`${url} did not start`) }
const until = async (what, seconds, read) => {
  for (let t = 0; t < seconds * 2; t++) { const value = await read().catch(() => null); if (value) return value; await sleep(500) }
  throw new Error(`${what}: not there after ${seconds} s`)
}
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b }

const state = join(chain, '.ledger')
rmSync(state, { recursive: true, force: true })
mkdirSync(state, { recursive: true })
const validator = spawn('bash', [join(chain, 'scripts/validator.sh'), state, '32', '--bpf-program', program, join(chain, 'target/deploy/acrux_bridge.so')], { stdio: 'ignore' })
const store = join(node, 'target/bridge-round-trip-check')
rmSync(store, { recursive: true, force: true })
mkdirSync(join(store, 'placements'), { recursive: true })
writeFileSync(join(store, 'bridge.json'), JSON.stringify({ program, fromSlot: 0 }))
spawnSync(process.execPath, ['scripts/place-bridge.mjs', join(store, 'placements/acrux-bridge.json')], { cwd: chain, stdio: 'inherit' })
writeFileSync(join(store, 'placements/placements.json'), JSON.stringify({ placements: [{ file: 'acrux-bridge.json', slot: 0 }] }))
const env = { ...process.env, ACRUX_BRIDGE: join(store, 'bridge.json'), ACRUX_BRIDGE_RPC: solanaUrl, ACRUX_PLACEMENTS: join(store, 'placements/placements.json') }
const net = spawn(acrux, ['net', 'up', '--store', join(store, 'net')], { cwd: node, env, stdio: 'ignore' })
let ok = false
try {
  await up(solanaUrl)
  await up(gateway)
  const solana = bridgeProgram(new Connection(solanaUrl, 'confirmed'), devKeypair('local-authority'))
  // The light client, seeded from the net's first finalized cblock, which also hands epoch 1 on.
  const first = await until('a cblock proof', 60, async () => { for (let c = 1; c < 9; c++) { const p = await cblockProof(gateway, c).catch(() => null); if (p) return p } })
  await seed(solana, first, Math.floor((first.consensusSlot * 4) / EPOCH_SLOTS))
  await relay(solana, first)

  // Solana to Acrux.
  const holder = Keypair.generate()
  const granted = await fetch('http://127.0.0.1:4989/internal/faucet', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ address: holder.publicKey.toBase58(), amountLamports: '2000000000' }) })
  if (!granted.ok) throw new Error(`faucet: ${await granted.text()}`)
  const deposited = spawnSync(process.execPath, ['scripts/bridge-deposit.mjs', '--recipient', holder.publicKey.toBase58(), '--amount', '2500000000'], { cwd: chain, encoding: 'utf8' })
  if (deposited.status !== 0) throw new Error(deposited.stderr)
  const record = JSON.parse(deposited.stdout.trim().split('\n').at(-1))
  const solanaMint = new PublicKey(record.mint)
  console.log(`deposit on Solana: ${record.amount} of ${record.mint} for ${holder.publicKey.toBase58()} (sequence ${record.sequence})`)
  // The bridged mint, derived from the Solana mint; the faucet's demo tokens sit beside it.
  const bridgedMint = PublicKey.findProgramAddressSync([Buffer.from('mint'), solanaMint.toBuffer()], solana.programId)[0]
  const credited = await until('the credit on Acrux', 90, async () => (await rpc(gateway, 'getTokenAccountsByOwner', [holder.publicKey.toBase58(), { mint: bridgedMint.toBase58() }, { encoding: 'jsonParsed' }]))?.value?.[0])
  console.log(`credited on Acrux: ${credited.account.data.parsed.info.tokenAmount.amount} at ${bridgedMint.toBase58()}`)

  // Acrux to Solana: the burn on Acrux, through the same program placed there.
  const acruxConnection = new Connection(gateway, 'confirmed')
  const onAcrux = bridgeProgram(acruxConnection, holder)
  const recipient = Keypair.generate().publicKey
  const outbound = PublicKey.findProgramAddressSync([Buffer.from('outbound')], onAcrux.programId)[0]
  const made = (await acruxConnection.getAccountInfo(outbound)) ? (await onAcrux.account.outbound.fetch(outbound)).withdrawals.toNumber() : 0
  const withdrawal = PublicKey.findProgramAddressSync([Buffer.from('withdrawal'), u64(made)], onAcrux.programId)[0]
  const burned = await onAcrux.methods.withdraw(solanaMint, new anchor.BN(1_000_000_000), recipient)
    .accountsPartial({ owner: holder.publicKey, mint: bridgedMint, source: new PublicKey(credited.pubkey), record: withdrawal, tokenProgram: TOKEN_PROGRAM_ID }).rpc()
  const w = await onAcrux.account.withdrawal.fetch(withdrawal)
  const cblock = Math.floor(w.slot.toNumber() / 4)
  const left = (await rpc(gateway, 'getTokenAccountBalance', [credited.pubkey])).value.amount
  console.log(`withdrawn on Acrux: 1000000000 burned in slot ${w.slot} (cblock ${cblock}), sequence ${w.sequence}, ${left} left on Acrux (${burned})`)

  // The cblock of the burn, certified on Solana, pays out of the vault.
  const proof = await until(`cblock ${cblock}'s proof`, 60, () => cblockProof(gateway, cblock))
  const vaultBefore = (await getAccount(solana.provider.connection, PublicKey.findProgramAddressSync([Buffer.from('vault'), solanaMint.toBuffer()], solana.programId)[0])).amount
  const relayed = await relay(solana, proof)
  console.log('relayed:', JSON.stringify({ slot: relayed.slot, epoch: relayed.epoch, certified: relayed.certified, released: relayed.released, transactions: relayed.transactions.length }))
  const paid = (await getAccount(solana.provider.connection, getAssociatedTokenAddressSync(solanaMint, recipient))).amount
  const vaultAfter = (await getAccount(solana.provider.connection, PublicKey.findProgramAddressSync([Buffer.from('vault'), solanaMint.toBuffer()], solana.programId)[0])).amount
  console.log(`paid on Solana: ${paid} to ${recipient.toBase58()}, vault ${vaultBefore} -> ${vaultAfter}`)
  // Relaying the same cblock again pays nothing more.
  const again = await relay(solana, proof)
  console.log(`relayed again: ${again.released.length} paid`)
  ok = relayed.certified && paid === 1_000_000_000n && vaultBefore - vaultAfter === 1_000_000_000n && left === '1500000000' && again.released.length === 0
} finally {
  net.kill()
  validator.kill('SIGTERM')
  await sleep(2000)
  rmSync(state, { recursive: true, force: true })
}
console.log(ok ? 'the token went to Acrux and came back to Solana, paid once' : 'FAILED')
process.exit(ok ? 0 : 1)
