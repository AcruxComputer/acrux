// Calls every API route in node mode through the contract's client, which parses each response
// with the contract's schemas. Usage: node scripts/node-check.mjs <api-url> [scenario]
import { createClient } from '@acrux/contract'
const api = createClient(process.argv[2] ?? 'http://127.0.0.1:4920')
const step = async (name, run) => { const t = Date.now(); try { const v = await run(); console.log('ok  ', name, `${Date.now() - t} ms`, v ?? '') ; return v } catch (e) { console.log('FAIL', name, e?.body ? JSON.stringify(e.body) : e?.message ?? e) } }
await step('health', async () => JSON.stringify(await api.health()))
const config = await step('registry config', async () => { const c = await api.registryConfig(); return `${c.status} epoch ${c.currentEpoch} deposit ${c.depositLamports}` })
await step('hubs', async () => (await api.hubs()).items.map(h => `${h.code}:${h.status}:${h.members.length}m:${h.proposerStake}`).join(' '))
await step('hub FRA', async () => { const h = await api.hub('FRA'); return `${h.region} ${h.latitude.toFixed(2)},${h.longitude.toFixed(2)} cap ${h.proposerStakeCap} needs ${JSON.stringify(h.needs)}` })
await step('schedule', async () => { const s = await api.schedule(); return `epoch ${s.epoch} windows ${s.totalWindows} shares ${s.shares.map(x => x.hubCode + ':' + x.assignedWindows).join(',')} runs ${s.regionalRuns.length}` })
await step('facts', async () => { const f = await api.facts(); return `${f.source} n=${f.sampleSize} pacing p99 ${f.slotPacingUs.p99} links ${Object.keys(f.handoverUs).length}` })
await step('spec', async () => `${(await api.spec()).entries.length} entries`)
const address = '7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU'
await step('faucet status', async () => JSON.stringify(await api.faucetStatus(address)).slice(0, 120))
const grant = await step('faucet', async () => { const g = await api.faucet({ address }); return g.transactionId })
if (grant) { await new Promise(r => setTimeout(r, 1500)); await step('transaction', async () => (await api.transaction(grant)).state) }
if (process.argv[3]) {
  const run = await step('lab start', async () => (await api.startLabRun({ scenario: process.argv[3] })).id)
  if (run) {
    for (;;) { const r = await api.labRun(run); if (['done', 'failed_to_start'].includes(r.status)) { console.log('     lab', r.status, r.evidence.map(e => e.kind).join(','), (r.failure ?? r.outcome ?? '').slice(0, 100)); break } await new Promise(r => setTimeout(r, 1000)) }
    await step('lab replay', async () => (await api.labReplay(run)).events.length + ' events')
  }
}
await step('lab runs', async () => (await api.labRuns()).items.length + ' runs')
