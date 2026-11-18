// Road build 1, step 6: the slot, measured the same way on Acrux and on Solana from one client.
// Stock @solana/web3.js `onSlotChange` (slotSubscribe) on both at once for N seconds: the gap
// between each slot and the next as the client hears them, median and p90. Beside it, each
// chain's own `getRecentPerformanceSamples` (numSlots over samplePeriodSecs).
// Usage: node test/step6-slots.mjs [out.json] [seconds] [acrux rpc] [solana rpc] [solana name]
// Only an RPC's host is printed or written, never its path or query (a keyed URL stays out).
import { writeFileSync } from 'node:fs'
import { Connection } from '@solana/web3.js'

const [
  out = 'step6-slots.json',
  seconds = '60',
  acruxRpc = 'https://rpc.acrux.computer',
  solanaRpc = 'https://api.mainnet-beta.solana.com',
  solanaName = 'solanaMainnet',
] = process.argv.slice(2)

const secret = new URL(solanaRpc).search
if (secret) {
  for (const method of ['log', 'error', 'warn']) {
    const write = console[method]
    console[method] = (...args) => write(...args.map(a => String(a?.stack ?? a).replaceAll(secret, '')))
  }
}

const sorted = values => [...values].sort((a, b) => a - b)
const quantile = (values, q) => Math.round(sorted(values)[Math.min(values.length - 1, Math.floor(q * values.length))] * 10) / 10
const median = values => {
  const s = sorted(values), mid = s.length >> 1
  return Math.round((s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2) * 10) / 10
}

const chains = [
  { name: 'acrux', rpc: acruxRpc },
  { name: solanaName, rpc: solanaRpc },
].map(chain => ({ ...chain, host: new URL(chain.rpc).host, connection: new Connection(chain.rpc, 'processed'), heard: new Map() }))

for (const chain of chains) {
  chain.subscription = chain.connection.onSlotChange(info => {
    if (!chain.heard.has(info.slot)) chain.heard.set(info.slot, performance.now())
  })
}
await new Promise(resolve => setTimeout(resolve, Number(seconds) * 1000))

const measuredAt = new Date().toISOString()
const result = {
  measuredAt,
  date: measuredAt.slice(0, 10),
  method: 'one client, stock @solana/web3.js onSlotChange (slotSubscribe) on both chains at once; the gap between each slot and the next as heard, and each chain\'s getRecentPerformanceSamples',
  seconds: Number(seconds),
}
for (const chain of chains) {
  await chain.connection.removeSlotChangeListener(chain.subscription)
  const slots = [...chain.heard.keys()].sort((a, b) => a - b)
  const gaps = []
  for (let i = 1; i < slots.length; i++) {
    if (slots[i] === slots[i - 1] + 1) gaps.push(chain.heard.get(slots[i]) - chain.heard.get(slots[i - 1]))
  }
  const samples = await chain.connection.getRecentPerformanceSamples(10).catch(() => [])
  const perSlot = samples.filter(s => s.numSlots > 0).map(s => (s.samplePeriodSecs * 1000) / s.numSlots)
  result[chain.name] = {
    rpcHost: chain.host,
    slotGapMs: { median: median(gaps), p90: quantile(gaps, 0.9), gaps: gaps.length },
    slotsHeard: slots.length,
    slotsAdvanced: slots.length ? slots.at(-1) - slots[0] : 0,
    performanceSamples: perSlot.length ? { msPerSlotMedian: median(perSlot), samples: perSlot.length, slots: samples.reduce((n, s) => n + s.numSlots, 0), seconds: samples.reduce((n, s) => n + s.samplePeriodSecs, 0) } : null,
  }
}
writeFileSync(out, JSON.stringify(result, null, 2) + '\n')
for (const chain of chains) {
  const r = result[chain.name]
  const samples = r.performanceSamples ? `, getRecentPerformanceSamples ${r.performanceSamples.msPerSlotMedian} ms a slot over ${r.performanceSamples.samples} samples` : ''
  console.log(`${chain.name}: slot gap median ${r.slotGapMs.median} ms, p90 ${r.slotGapMs.p90} ms over ${r.slotGapMs.gaps} gaps${samples} (${chain.host})`)
}
console.log(`written ${out}`)
process.exit(0)
