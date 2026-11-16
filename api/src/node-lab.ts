// Fault Lab runs on the node's runner, one at a time. The runner (`acrux lab run`) appends protocol
// events to events.jsonl while the sandbox runs and writes outcome.json at the end; every event and
// the outcome go to Postgres, so a run replays after its directory is gone.
import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'
import type { Evidence, LabRun, LabScenario, ProtocolEvent } from '@acrux/contract'
import { ProtocolEvent as ProtocolEventSchema } from '@acrux/contract'
import { scenarios } from '@acrux/contract/fixtures'
import type { PrismaClient } from '@prisma/client'
import { env } from './config.js'

type Listener = (event: ProtocolEvent) => void
type Outcome = { status: 'done' | 'failed'; outcome: string | null; evidence: Evidence[]; failure: string | null }
type RunRow = Awaited<ReturnType<PrismaClient['labRun']['findFirstOrThrow']>>

export class NodeLab {
  private queue: string[] = []
  /** Runs waiting or running; a public host caps this so sandboxes never pile up. */
  get pending() { return this.queue.length + (this.running ? 1 : 0) }
  private running = false
  private listeners = new Map<string, Set<Listener>>()
  constructor(private db: PrismaClient) {}

  scenario(id: string): LabScenario { return scenarios.find(s => s.id === id)! }

  view(row: RunRow): LabRun {
    const result = row.outcome as { outcome: string | null; evidence: Evidence[] } | null
    return { id: row.id, scenario: this.scenario(row.scenario), seed: row.seed, status: row.status,
      createdAt: row.createdAt.toISOString(), startedAt: row.startedAt?.toISOString() ?? null, endedAt: row.finishedAt?.toISOString() ?? null,
      outcome: result?.outcome ?? null, evidence: result?.evidence ?? [], failure: (row.failure as string | null) ?? null }
  }

  async start(scenario: string, seed: string) {
    const row = await this.db.labRun.create({ data: { scenario, seed, parameters: this.scenario(scenario).parameters } })
    await this.record(row.id, this.updated(this.view(row)))
    this.queue.push(row.id)
    void this.drain()
    return this.view(row)
  }

  async events(runId: string): Promise<ProtocolEvent[]> {
    const rows = await this.db.labRunEvent.findMany({ where: { runId }, orderBy: { sequence: 'asc' } })
    return rows.map(r => r.event as ProtocolEvent)
  }

  listen(runId: string, listener: Listener) {
    const set = this.listeners.get(runId) ?? new Set()
    set.add(listener)
    this.listeners.set(runId, set)
    return () => set.delete(listener)
  }

  private updated(run: LabRun): Omit<ProtocolEvent, 'sequence'> {
    return { version: 1, type: 'lab.updated', streamId: `lab-${run.id}`, slot: null, anchor: 'stream', atUs: 0, data: run } as Omit<ProtocolEvent, 'sequence'>
  }

  /** Appends events in order under the run's own sequence and tells the run's listeners. */
  private async record(runId: string, ...events: Omit<ProtocolEvent, 'sequence'>[]) {
    if (!events.length) return
    const next = await this.db.labRunEvent.count({ where: { runId } })
    const numbered = events.map((e, i) => ({ ...e, streamId: `lab-${runId}`, sequence: next + i }) as ProtocolEvent)
    await this.db.labRunEvent.createMany({ data: numbered.map(e => ({ runId, sequence: e.sequence, type: e.type, relativeUs: BigInt(e.atUs), event: e as object })) })
    for (const event of numbered) for (const listener of this.listeners.get(runId) ?? []) listener(event)
  }

  private async drain() {
    if (this.running) return
    this.running = true
    try {
      for (let id = this.queue.shift(); id; id = this.queue.shift()) await this.execute(id)
    } finally { this.running = false }
  }

  private async execute(id: string) {
    const dir = join(env.LAB_DIR, id)
    mkdirSync(dir, { recursive: true })
    let row = await this.db.labRun.update({ where: { id }, data: { status: 'running', startedAt: new Date() } })
    await this.record(id, this.updated(this.view(row)))
    const child = spawn(env.ACRUX_BIN, ['lab', 'run', '--scenario', row.scenario, '--seed', row.seed, '--base-port', String(env.LAB_BASE_PORT), '--out', dir, '--genesis', env.ACRUX_GENESIS],
      { cwd: env.ACRUX_NODE_DIR, stdio: ['ignore', 'ignore', 'pipe'] })
    let stderr = ''
    child.stderr.on('data', d => { stderr = (stderr + d).slice(-2000) })
    // Tail events.jsonl while the sandbox runs.
    let offset = 0
    const tail = async () => {
      const path = join(dir, 'events.jsonl')
      if (!existsSync(path)) return
      const text = readFileSync(path, 'utf8')
      const end = text.lastIndexOf('\n') + 1
      if (end <= offset) return
      const lines = text.slice(offset, end).split('\n').filter(Boolean)
      offset = end
      await this.record(id, ...lines.map(l => ProtocolEventSchema.parse(JSON.parse(l))))
    }
    const exited = new Promise<number | null>(resolve => { child.on('close', resolve); child.on('error', () => resolve(null)) })
    let code: number | null | undefined
    while (code === undefined) {
      code = await Promise.race([exited, new Promise<undefined>(r => setTimeout(r, 250))])
      await tail()
    }
    const outcomePath = join(dir, 'outcome.json')
    const outcome: Outcome | null = existsSync(outcomePath) ? JSON.parse(readFileSync(outcomePath, 'utf8')) : null
    const failed = code !== 0 || !outcome
    row = await this.db.labRun.update({ where: { id }, data: {
      status: failed && !outcome ? 'failed_to_start' : 'done', finishedAt: new Date(),
      outcome: outcome ? { outcome: outcome.outcome, evidence: outcome.evidence } : undefined,
      failure: outcome?.failure ?? (failed ? `The sandbox stopped (exit ${code}). ${stderr.trim().split('\n').pop() ?? ''}`.trim() : undefined) } })
    await this.record(id, this.updated(this.view(row)))
    rmSync(dir, { recursive: true, force: true })
  }
}
