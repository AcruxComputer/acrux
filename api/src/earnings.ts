// The node persists fee recipients with their historical epoch/hub labels at finalization.
// Closed epochs are immutable; the newest retained epoch is always read again.
import { rpcMethods, type HubEarnings, type HubEarningsEpoch } from '@acrux/contract'
import { fail } from './engines/server/errors.js'

const Epochs = rpcMethods.getValidatorEarningsEpochs.result
const Earnings = rpcMethods.getValidatorEarnings.result
type Epoch = ReturnType<typeof Earnings.parse>

export function createHubEarningsReader(rpcUrl: string) {
  const fixed = new Map<number, Epoch>()
  const pending = new Map<number, Promise<Epoch>>()
  let epochSlots: number | undefined
  async function rpc(method: string, params: unknown): Promise<unknown> {
    const response = await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }), signal: AbortSignal.timeout(5000) })
    if (!response.ok) throw new Error('Validator earnings read failed.')
    const body = await response.json() as { result?: unknown; error?: unknown } | null
    if (!body || body.error || !Object.hasOwn(body, 'result')) throw new Error('Validator earnings read failed.')
    return body.result
  }
  async function readEpoch(epoch: number, slots: number, closed: boolean): Promise<Epoch> {
    if (fixed.has(epoch)) return fixed.get(epoch)!
    if (closed && pending.has(epoch)) return pending.get(epoch)!
    const read = (async () => {
      const value = Earnings.parse(await rpc('getValidatorEarnings', { epoch }))
      const firstSlot = epoch * slots
      if (value.epoch !== epoch || !Number.isSafeInteger(firstSlot) || value.countedFromSlot < firstSlot
        || value.countedFromSlot - firstSlot >= slots || new Set(value.rows.map(row => row.identity)).size !== value.rows.length) {
        throw new Error('Invalid validator earnings epoch.')
      }
      return value
    })()
    // A running read must never fill the immutable cache after an epoch boundary.
    if (closed) pending.set(epoch, read)
    try { return await read }
    finally { if (pending.get(epoch) === read) pending.delete(epoch) }
  }
  return async (hubCode: string): Promise<HubEarnings> => {
    try {
      const list = Epochs.parse(await rpc('getValidatorEarningsEpochs', {}))
      if (list.epochs.some((epoch, index) => index > 0 && epoch >= list.epochs[index - 1]!)) throw new Error('Invalid validator earnings order.')
      if (epochSlots !== list.epochSlots) { fixed.clear(); epochSlots = list.epochSlots }
      const retained = new Set(list.epochs)
      for (const epoch of fixed.keys()) if (!retained.has(epoch)) fixed.delete(epoch)
      const newest = list.epochs[0]
      const epochs = await Promise.all(list.epochs.map(async epoch => {
        const value = await readEpoch(epoch, list.epochSlots, epoch !== newest)
        if (epoch !== newest) fixed.set(epoch, value)
        const rows = value.rows.filter(row => row.hub === hubCode).map(({ hub: _hub, ...row }) => row)
        return { epoch, countedFromSlot: value.countedFromSlot, rows } satisfies HubEarningsEpoch
      }))
      return { hubCode, epochSlots: list.epochSlots, epochs }
    } catch {
      fail('NET_UNAVAILABLE', 'Acrux validator earnings are unavailable.', { retryable: true, stage: 'read' })
    }
  }
}
