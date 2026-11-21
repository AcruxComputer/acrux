import { z } from 'zod'
import { createHttp, type HttpOptions, type CallOptions } from './http.js'
import { ApiError, Health, Hubs, Hub, HubEarnings, GenesisShare, Rules, Member, RegistryConfig, SchedulePreview, Facts, SpecMap, Transaction, FaucetRequest, FaucetGrant, FaucetStatus, LabScenario, LabRunRequest, LabRun, LabRuns, PageQuery, Buyback, Burn, ReplayRange, ReplaySlotRecord, Bridge, BridgeTransfers } from './schemas.js'
import { LabReplay } from './events.js'
export function createClient(baseUrl: string, options: Omit<HttpOptions, 'apiError'> = {}) {
  const http = createHttp(baseUrl, { ...options, apiError: ApiError })
  const path = encodeURIComponent
  return {
    health: (call?: CallOptions) => http.get('/health', Health, call),
    burn: (query: z.input<typeof PageQuery> = {}, call?: CallOptions) => http.get('/v1/burn', Burn, { ...call, query }),
    buyback: (call?: CallOptions) => http.get('/v1/buyback', Buyback, call),
    bridge: (call?: CallOptions) => http.get('/v1/bridge', Bridge, call),
    bridgeTransfers: (query: { address?: string; kind?: 'deposit' | 'withdrawal'; cursor?: string; limit?: number } = {}, call?: CallOptions) => http.get('/v1/bridge/transfers', BridgeTransfers, { ...call, query }),
    replay: (range: { from?: number; to?: number } = {}, call?: CallOptions) => http.get('/v1/replay', ReplayRange, { ...call, query: range }),
    replaySlot: (slot: number, call?: CallOptions) => http.get(`/v1/replay/${slot}`, ReplaySlotRecord, call),
    hubs: (query: z.input<typeof PageQuery> = {}, call?: CallOptions) => http.get('/v1/hubs', Hubs, { ...call, query }),
    hub: (code: string, call?: CallOptions) => http.get(`/v1/hubs/${path(code)}`, Hub, call),
    hubEarnings: (code: string, call?: CallOptions) => http.get(`/v1/hubs/${path(code)}/earnings`, HubEarnings, call),
    genesisShare: (call?: CallOptions) => http.get('/v1/genesis-share', GenesisShare, call),
    rules: (call?: CallOptions) => http.get('/v1/rules', Rules, call),
    registryConfig: (call?: CallOptions) => http.get('/v1/registry/config', RegistryConfig, call),
    member: (identity: string, call?: CallOptions) => http.get(`/v1/registry/members/${path(identity)}`, Member, call),
    schedule: (epoch?: number, call?: CallOptions) => http.get('/v1/schedule', SchedulePreview, { ...call, query: { epoch } }),
    facts: (call?: CallOptions) => http.get('/v1/facts', Facts, call),
    spec: (call?: CallOptions) => http.get('/v1/spec', SpecMap, call),
    transaction: (id: string, call?: CallOptions) => http.get(`/v1/transactions/${path(id)}`, Transaction, call),
    faucetStatus: (address: string, call?: CallOptions) => http.get(`/v1/faucet/${path(address)}`, FaucetStatus, call),
    faucet: (body: z.input<typeof FaucetRequest>, call?: CallOptions) => http.post('/v1/faucet', FaucetGrant, FaucetRequest.parse(body), call),
    labScenarios: (call?: CallOptions) => http.get('/v1/lab/scenarios', z.array(LabScenario), call),
    labRuns: (query: z.input<typeof PageQuery> = {}, call?: CallOptions) => http.get('/v1/lab/runs', LabRuns, { ...call, query }),
    startLabRun: (body: z.input<typeof LabRunRequest>, call?: CallOptions) => http.post('/v1/lab/runs', LabRun, LabRunRequest.parse(body), call),
    labRun: (id: string, call?: CallOptions) => http.get(`/v1/lab/runs/${path(id)}`, LabRun, call),
    labReplay: (id: string, call?: CallOptions) => http.get(`/v1/lab/runs/${path(id)}/events`, LabReplay, call),
    labStreamUrl: (id: string) => http.streamUrl(`/v1/lab/runs/${path(id)}/stream`),
  }
}