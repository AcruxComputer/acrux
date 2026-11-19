// The node calculates percentiles from bounded raw samples; this read model only joins members.
import { HashExchangeTelemetry, type HashExchange, type HubHashExchange } from '@acrux/contract'
import { fail } from './engines/server/errors.js'

export type EffectiveHubMembers = ReadonlyMap<string, ReadonlySet<string>>

export function createHashExchangeReader(internalUrl: string) {
  let cached: HashExchangeTelemetry | undefined
  let expiresAt = 0
  let pending: Promise<HashExchangeTelemetry> | undefined
  return {
    async read(): Promise<HashExchangeTelemetry> {
      if (cached && Date.now() < expiresAt) return cached
      if (pending) return pending
      const read = (async () => {
        try {
          const response = await fetch(new URL('/internal/hash-exchange', internalUrl), { signal: AbortSignal.timeout(2000) })
          if (!response.ok) throw new Error('Hash exchange read failed.')
          const value = HashExchangeTelemetry.parse(await response.json())
          const hubs = new Set<string>()
          for (const hub of value.hubs) {
            if (hubs.has(hub.hubCode)) throw new Error('Duplicate hash exchange hub.')
            hubs.add(hub.hubCode)
            const pairs = new Set<string>()
            for (const pair of hub.pairs) {
              const key = pair.members.join(':')
              if (pairs.has(key)) throw new Error('Duplicate hash exchange pair.')
              pairs.add(key)
            }
          }
          cached = value
          expiresAt = Date.now() + 2000
          return value
        } catch {
          fail('NET_UNAVAILABLE', 'Acrux hash-exchange measurements are unavailable.', { retryable: true, stage: 'read' })
        }
      })()
      pending = read
      try { return await read }
      finally { if (pending === read) pending = undefined }
    },
  }
}

/** Keep only currently effective members of the hub named by the node. Spread summaries remain
 * the node's raw-sample aggregates; pair percentiles are never combined into new percentiles. */
export function memberHashExchange(telemetry: HashExchangeTelemetry, members: EffectiveHubMembers): HashExchange {
  return {
    measuredAt: telemetry.measuredAt, measurement: telemetry.measurement,
    hubs: telemetry.hubs.filter(hub => members.has(hub.hubCode)).map(hub => ({ ...hub,
      pairs: hub.pairs.filter(pair => pair.members.every(identity => members.get(hub.hubCode)!.has(identity))),
    })),
  }
}

export function hubHashExchange(telemetry: HashExchangeTelemetry, members: EffectiveHubMembers, code: string): HubHashExchange {
  const hub = memberHashExchange(telemetry, members).hubs.find(hub => hub.hubCode === code)
  // In a successfully read aggregate, an absent hub has no rolling observations.
  return { measuredAt: telemetry.measuredAt, measurement: telemetry.measurement,
    pairs: hub?.pairs ?? [], commitSpreadUs: hub?.commitSpreadUs ?? { applied: null, real: null } }
}
