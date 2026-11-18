// Road build 1, step 6: confirmation, measured. One client sends the same plain transfer with stock
// @solana/web3.js sendAndConfirmTransaction, alternating between Acrux and a Solana cluster,
// and writes each side's median and p90 beside the client's round trip to each RPC.
// Usage: node test/step6-confirm.mjs [out.json] [count] [acrux rpc] [acrux faucet] [solana rpc] [solana name] [funded solana keypair file] [funded acrux keypair file]
// The Acrux payer is a fresh in-memory key funded by the faucet, or a funded keypair file. On Solana, a keypair file sends to itself;
// without one, a fresh key is airdropped (clusters whose RPC faucet is open) and sends to a fresh recipient.
// SOLANA_COMMITMENT and ACRUX_COMMITMENT set each side's commitment (default confirmed; Acrux's confirmed already
// waits for the final cblock, so finalized is its like on Solana, and processed is executed on both). ONLY=acrux or ONLY=solana runs one side.
// Only an RPC's host is printed or written, never its path or query (a keyed URL stays out of the output).
import { readFileSync, writeFileSync } from 'node:fs'
import { Connection, Keypair, LAMPORTS_PER_SOL, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'

const [
  out = 'step6-confirmation.json',
  count = '50',
  acruxRpc = 'https://rpc.acrux.computer',
  faucet = 'https://api.acrux.computer/v1/faucet',
  solanaRpc = 'https://api.mainnet-beta.solana.com',
  solanaName = 'solanaMainnet',
  solanaKey,
  acruxKey,
] = process.argv.slice(2)
const n = Number(count)
const solanaCommitment = process.env.SOLANA_COMMITMENT ?? 'confirmed'
const acruxCommitment = process.env.ACRUX_COMMITMENT ?? 'confirmed'
const only = process.env.ONLY
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))

const secret = new URL(solanaRpc).search
if (secret) {
  for (const method of ['log', 'error', 'warn']) {
    const write = console[method]
    console[method] = (...args) => write(...args.map(a => String(a?.stack ?? a).replaceAll(secret, '')))
  }
}

const load = file => (file ? Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(file, 'utf8')))) : Keypair.generate())
const chains = [
  { name: 'acrux', side: 'acrux', rpc: acruxRpc, commitment: acruxCommitment, payer: load(acruxKey) },
  { name: solanaName, side: 'solana', rpc: solanaRpc, commitment: solanaCommitment, payer: load(solanaKey) },
].filter(chain => !only || chain.side === only)
for (const chain of chains) {
  chain.host = new URL(chain.rpc).host
  chain.connection = new Connection(chain.rpc, chain.commitment)
  chain.self = chain.side === 'solana' && Boolean(solanaKey)
  chain.to = chain.self ? chain.payer.publicKey : Keypair.generate().publicKey
  chain.confirmMs = []
  chain.signatures = []
}

const acrux = chains.find(chain => chain.side === 'acrux')
const solana = chains.find(chain => chain.side === 'solana')
if (acrux && !acruxKey) {
  const grant = await fetch(faucet, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ address: acrux.payer.publicKey.toBase58() }) })
  if (!grant.ok) throw new Error(`faucet refused: ${await grant.text()}`)
}
if (solana && !solanaKey) await solana.connection.requestAirdrop(solana.payer.publicKey, LAMPORTS_PER_SOL)
for (const chain of chains) {
  while ((await chain.connection.getBalance(chain.payer.publicKey)) === 0) await sleep(500)
  // A fresh recipient is funded once above rent, so every measured transfer is the same small one.
  if (!chain.self) {
    const rent = await chain.connection.getMinimumBalanceForRentExemption(0)
    await sendAndConfirmTransaction(chain.connection, transfer(chain, rent), [chain.payer], { commitment: 'confirmed' })
  }
  console.log(`${chain.name} (${chain.host}): payer ${chain.payer.publicKey.toBase58()}, recipient ${chain.self ? 'itself' : chain.to.toBase58()}`)
}

function transfer(chain, lamports) {
  return new Transaction().add(SystemProgram.transfer({ fromPubkey: chain.payer.publicKey, toPubkey: chain.to, lamports }))
}

async function roundTrips(chain, times = 15) {
  const ms = []
  for (let i = 0; i < times; i++) {
    const started = performance.now()
    await chain.connection.getSlot()
    ms.push(performance.now() - started)
  }
  return ms
}

const rtt = {}
for (const chain of chains) rtt[chain.name] = await roundTrips(chain)

// Alternating, one send at a time with a pause between, so both sides see the same client and the same hour.
for (let i = 0; i < n; i++) {
  for (const chain of chains) {
    const started = performance.now()
    const signature = await sendAndConfirmTransaction(chain.connection, transfer(chain, 1000 + i), [chain.payer], { commitment: chain.commitment })
    chain.confirmMs.push(performance.now() - started)
    chain.signatures.push(signature)
    await sleep(500)
  }
  if ((i + 1) % 10 === 0) console.log(`${i + 1}/${n}`)
}
for (const chain of chains) rtt[chain.name].push(...(await roundTrips(chain)))

const sorted = values => [...values].sort((a, b) => a - b)
const quantile = (values, q) => Math.round(sorted(values)[Math.min(values.length - 1, Math.floor(q * values.length))])
const median = values => {
  const s = sorted(values), mid = s.length >> 1
  return Math.round(s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2)
}

const measuredAt = new Date().toISOString()
const result = {
  measuredAt,
  date: measuredAt.slice(0, 10),
  method: 'one client, stock @solana/web3.js sendAndConfirmTransaction at the commitment each side names, the same plain SOL transfer, alternating between the RPCs, one send at a time',
  count: n,
}
for (const chain of chains) {
  result[chain.name] = {
    rpcHost: chain.host,
    commitment: chain.commitment,
    confirmMs: { median: median(chain.confirmMs), p90: quantile(chain.confirmMs, 0.9), min: Math.round(Math.min(...chain.confirmMs)), max: Math.round(Math.max(...chain.confirmMs)) },
    roundTripMs: { median: median(rtt[chain.name]), samples: rtt[chain.name].length },
    signatures: chain.signatures,
  }
}
writeFileSync(out, JSON.stringify(result, null, 2) + '\n')
for (const chain of chains) {
  const r = result[chain.name]
  console.log(`${chain.name}: ${chain.commitment} median ${r.confirmMs.median} ms, p90 ${r.confirmMs.p90} ms, round trip ${r.roundTripMs.median} ms (${chain.host})`)
}
console.log(`written ${out}`)
process.exit(0)
