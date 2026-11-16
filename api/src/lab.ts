import { randomUUID } from 'node:crypto'
import type { LabRun, ProtocolEvent } from '@acrux/contract'
import { labRuns, makeLabReplay, replayOffsetUs } from '@acrux/contract/fixtures'

type Record = { run: LabRun; events: ProtocolEvent[]; listeners: Set<(event: ProtocolEvent) => void> }

export class FixtureLab {
  private records = new Map<string, Record>(labRuns.map(run => [run.id, { run, events: makeLabReplay(run).events, listeners: new Set() }]))
  private timers = new Set<NodeJS.Timeout>()

  list() { return [...this.records.values()].map(record => record.run).reverse() }
  get(id: string) { return this.records.get(id) }

  start(template: LabRun, seed: string, failed: boolean) {
    const createdAt = new Date().toISOString()
    const run: LabRun = { ...template, id: randomUUID(), seed, createdAt, startedAt: null, endedAt: failed ? createdAt : null,
      status: failed ? 'failed_to_start' : 'queued', outcome: null, evidence: [], failure: failed ? 'The fixture sandbox could not acquire a runner.' : null }
    const record: Record = { run, events: [], listeners: new Set() }
    this.records.set(run.id, record)
    this.publish(record, { version: 1, type: 'lab.updated', streamId: `lab-${run.id}`, sequence: 0, slot: null, anchor: 'stream', atUs: 0, data: run })
    if (!failed) this.later(100, () => {
      const finalRun: LabRun = { ...template, id: run.id, seed, createdAt, startedAt: new Date().toISOString() }
      const replay = makeLabReplay(finalRun)
      const firstSlot = Math.min(...replay.events.flatMap(event => event.slot === null ? [] : [event.slot]))
      finalRun.endedAt = new Date(Date.now() + Math.max(...replay.events.map(event => replayOffsetUs(event, firstSlot))) / 1000).toISOString()
      for (const event of replay.events) this.later(replayOffsetUs(event, firstSlot) / 1000, () => {
        const next = { ...event, sequence: record.events.length }
        if (next.type === 'lab.updated') record.run = next.data
        this.publish(record, next)
      })
    })
    return run
  }

  private publish(record: Record, event: ProtocolEvent) {
    record.events.push(event)
    for (const listener of record.listeners) listener(event)
  }

  private later(ms: number, action: () => void) {
    const timer = setTimeout(() => { this.timers.delete(timer); action() }, ms)
    this.timers.add(timer)
  }

  close() { for (const timer of this.timers) clearTimeout(timer); this.timers.clear() }
}
