// Loads replayed Solana mainnet slots into the API's database, which serves them at /v1/replay:
// for each slot of the range it runs `acrux replay --json` against SOLANA_RPC_URL (the shared
// Helius key; one getBlock a slot) and stores the record. `--parallel` workers each pause between
// slots. Slots mainnet skipped are passed over; a slot already stored is read again only with
// --again. With `--whole sandwiches` (an epoch) a slot without sandwiches keeps only its counts.
// Usage: node scripts/replay-load.mjs --from <slot> --count <n> [--parallel 1] [--pause-ms 250]
//        [--whole all|sandwiches] [--again] [--acrux <binary>]
import { spawn } from 'node:child_process'
import { existsSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { Prisma } from '@prisma/client'
import { connectDatabase } from '../dist/db.js'
import { ReplaySlotRecord } from '@acrux/contract'

if (existsSync('.env')) process.loadEnvFile('.env')
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`)
  return i > 0 ? process.argv[i + 1] : fallback
}
const from = Number(arg('from'))
const count = Number(arg('count', '1'))
const parallel = Number(arg('parallel', '1'))
const pause = Number(arg('pause-ms', '250'))
const whole = arg('whole', 'all')
const again = process.argv.includes('--again')
const acrux = arg('acrux', process.env.ACRUX_BIN ?? (existsSync('/app/node/acrux') ? '/app/node/acrux' : '../node/target/release/acrux.exe'))
const rpc = process.env.SOLANA_RPC_URL
if (!Number.isSafeInteger(from) || !(count > 0) || !(parallel > 0) || !['all', 'sandwiches'].includes(whole) || !rpc) {
  console.error('Usage: node scripts/replay-load.mjs --from <slot> --count <n> [--parallel n] [--whole all|sandwiches], with SOLANA_RPC_URL and DATABASE_URL set')
  process.exit(1)
}
const host = new URL(rpc).host
const database = connectDatabase(process.env.DATABASE_URL)
const db = database.db
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const tally = { loaded: 0, sandwiches: 0, skipped: 0, present: 0, failed: 0 }
const replay = (slot, out) => new Promise(resolve => {
  const child = spawn(acrux, ['replay', '--rpc', rpc, '--slot', String(slot), '--rows', '0', '--json', out])
  let text = ''
  child.stdout.on('data', d => { text = (text + d).slice(-2000) })
  child.stderr.on('data', d => { text = (text + d).slice(-2000) })
  child.on('close', status => resolve({ status, message: text.replaceAll(rpc, host).trim().split('\n').at(-1) ?? '' }))
})

let next = from
let done = 0
async function worker(id) {
  const out = join(tmpdir(), `acrux-replay-${process.pid}-${id}.json`)
  for (let slot = next++; slot < from + count; slot = next++) {
    if (!again && (await db.replaySlot.findUnique({ where: { slot: BigInt(slot) }, select: { slot: true } }))) tally.present++
    else {
      const { status, message } = await replay(slot, out)
      if (status !== 0) {
        if (/skipped|missing|not available|-32007|-32009/i.test(message)) tally.skipped++
        else {
          tally.failed++
          console.error(`slot ${slot}: ${message}`)
        }
      } else {
        const record = ReplaySlotRecord.parse(JSON.parse(readFileSync(out, 'utf8')))
        const row = { transactions: record.transactions, acruxSlots: record.acruxSlots.length, sandwiches: record.sandwiches, record: whole === 'all' || record.sandwiches.length ? record : Prisma.DbNull }
        await db.replaySlot.upsert({ where: { slot: BigInt(slot) }, create: { slot: BigInt(slot), ...row }, update: { ...row, readAt: new Date() } })
        tally.loaded++
        tally.sandwiches += record.sandwiches.length
        if (record.sandwiches.length) console.log(`slot ${slot}: ${record.sandwiches.length} sandwich(es)`)
      }
      await sleep(pause)
    }
    if (++done % 500 === 0) console.log(`${done}/${count}`, JSON.stringify(tally))
  }
  rmSync(out, { force: true })
}
await Promise.all(Array.from({ length: parallel }, (_, i) => worker(i)))
console.log(`slots ${from} to ${from + count - 1} from ${host}:`, JSON.stringify(tally))
await database.close()
