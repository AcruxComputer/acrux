// Algorithm 3 through the node's own binary (`acrux schedule`), so the preview and the net's
// schedule are one code path. Stakes cross the boundary in units of 1,000 lamports.
import { spawn } from 'node:child_process'
import type { SchedulePreview } from '@acrux/contract'
import { env } from './config.js'

type HubInput = { code: string; region: string; members: { identity: string; stake: number; validatorKind?: 'genesis' | 'external' }[] }
type HubStake = { code: string; region: string; cap: number; stake: number; members: { identity: string; stake: number; proposerStake: number; capped: boolean }[] }
type NodeSchedule = { epoch: number; seed: string; alpha: number; shares: SchedulePreview['shares']; regionalRuns: SchedulePreview['regionalRuns']; windows: { number: number; hubCode: string; region: string; proposers: string[] }[] }

/** Windows per epoch in the preview, the node's own default. */
export const WINDOWS_PER_EPOCH = 96
export const SCHEDULE_SEED = 'acrux-schedule-v1'

function run(input: object): Promise<{ hubStakes: HubStake[]; schedule: NodeSchedule | null }> {
  return new Promise((resolve, reject) => {
    const child = spawn(env.ACRUX_BIN, ['schedule'], { stdio: ['pipe', 'pipe', 'pipe'] })
    let out = '', err = ''
    child.stdout.on('data', d => { out += d })
    child.stderr.on('data', d => { err += d })
    child.on('error', reject)
    child.on('close', code => (code === 0 ? resolve(JSON.parse(out)) : reject(new Error(`acrux schedule exited ${code}: ${err.trim()}`))))
    child.stdin.end(JSON.stringify(input))
  })
}

export async function hubStakes(hubs: HubInput[]) {
  return (await run({ epoch: 0, seed: SCHEDULE_SEED, windows: 1, totalStake: 1, hubs })).hubStakes
}

/** Algorithm 3's preview for an epoch; stakes and totalStake in units of 1,000 lamports. */
export async function schedulePreview(epoch: number, hubs: HubInput[], totalStake: number): Promise<SchedulePreview> {
  const { hubStakes: stakes, schedule } = await run({ epoch, seed: SCHEDULE_SEED, windows: WINDOWS_PER_EPOCH, totalStake, hubs })
  const lamports = (units: number) => (BigInt(units) * 1_000n).toString()
  if (!schedule) return { epoch, seed: SCHEDULE_SEED, source: 'registry', algorithm: 3, alpha: 0.5, totalWindows: 0, shares: [], regionalRuns: [], windows: [] }
  const member = (code: string, identity: string) => stakes.find(h => h.code === code)!.members.find(m => m.identity === identity)!
  return {
    epoch, seed: SCHEDULE_SEED, source: 'registry', algorithm: 3, alpha: schedule.alpha, totalWindows: schedule.windows.length,
    shares: schedule.shares.map(s => ({ ...s, proposerStake: lamports(Number(s.proposerStake)) })),
    regionalRuns: schedule.regionalRuns,
    windows: schedule.windows.map(w => ({
      number: w.number, hubCode: w.hubCode, firstSlot: w.number * 16, lastSlot: w.number * 16 + 15,
      consensusSlots: [0, 1, 2, 3].map(i => w.number * 4 + i),
      proposers: w.proposers.map((identity, i) => ({ index: i + 1, identity, stake: lamports(member(w.hubCode, identity).proposerStake), status: 'online' as const,
        validatorKind: hubs.find(hub => hub.code === w.hubCode)?.members.find(member => member.identity === identity)?.validatorKind })),
    })),
  }
}
