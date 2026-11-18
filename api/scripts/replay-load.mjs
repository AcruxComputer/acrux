// Loads replayed Solana mainnet slots into the API's database, which serves them at /v1/replay:
// for each slot of the range it runs `acrux replay --json` against SOLANA_RPC_URL (the shared
// Helius key; one getBlock a slot, a pause between) and stores the record. Slots mainnet skipped
// are passed over; a slot already stored is read again only with --again.
// Usage: node scripts/replay-load.mjs --from <slot> --count <n> [--pause-ms 250] [--again] [--acrux <binary>]
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { connectDatabase } from '../dist/db.js'
import { ReplaySlotRecord } from '@acrux/contract'

if (existsSync('.env')) process.loadEnvFile('.env')
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`)
  return i > 0 ? process.argv[i + 1] : fallback
}
const from = Number(arg('from'))
const count = Number(arg('count', '1'))
const pause = Number(arg('pause-ms', '250'))
const again = process.argv.includes('--again')
const acrux = arg('acrux', process.env.ACRUX_BIN ?? (existsSync('/app/node/acrux') ? '/app/node/acrux' : '../node/target/release/acrux.exe'))
const rpc = process.env.SOLANA_RPC_URL
if (!Number.isSafeInteger(from) || !(count > 0) || !rpc) {
  console.error('Usage: node scripts/replay-load.mjs --from <slot> --count <n>, with SOLANA_RPC_URL and DATABASE_URL set')
  process.exit(1)
}
const host = new URL(rpc).host
const database = connectDatabase(process.env.DATABASE_URL)
const db = database.db
const out = join(tmpdir(), `acrux-replay-${process.pid}.json`)
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const tally = { loaded: 0, sandwiches: 0, skipped: 0, present: 0, failed: 0 }

for (let slot = from; slot < from + count; slot++) {
  if (!again && (await db.replaySlot.findUnique({ where: { slot: BigInt(slot) }, select: { slot: true } }))) {
    tally.present++
    continue
  }
  const run = spawnSync(acrux, ['replay', '--rpc', rpc, '--slot', String(slot), '--rows', '0', '--json', out], { encoding: 'utf8' })
  if (run.status !== 0) {
    const message = (run.stderr || run.stdout || '').replaceAll(rpc, host).trim().split('\n').at(-1)
    if (/skipped|missing|not available|-32007|-32009/i.test(message)) tally.skipped++
    else {
      tally.failed++
      console.error(`slot ${slot}: ${message}`)
    }
  } else {
    const record = ReplaySlotRecord.parse(JSON.parse(readFileSync(out, 'utf8')))
    await db.replaySlot.upsert({
      where: { slot: BigInt(slot) },
      create: { slot: BigInt(slot), transactions: record.transactions, acruxSlots: record.acruxSlots.length, sandwiches: record.sandwiches, record },
      update: { transactions: record.transactions, acruxSlots: record.acruxSlots.length, sandwiches: record.sandwiches, record, readAt: new Date() },
    })
    tally.loaded++
    tally.sandwiches += record.sandwiches.length
    if (record.sandwiches.length) console.log(`slot ${slot}: ${record.sandwiches.length} sandwich(es)`)
  }
  if ((slot - from + 1) % 50 === 0) console.log(`${slot - from + 1}/${count}`, JSON.stringify(tally))
  await sleep(pause)
}
rmSync(out, { force: true })
console.log(`slots ${from} to ${from + count - 1} from ${host}:`, JSON.stringify(tally))
await database.close()
