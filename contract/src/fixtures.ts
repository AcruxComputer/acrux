import { FAUCET_RETIRED_MESSAGE } from './bridge.js'
import type { z } from 'zod'
import { toBase58, toBase64 } from './encoding.js'
import { PROTOCOL_PARAMETERS, errors } from './schemas.js'
import type { Balance, GenesisShare, HubEarnings, Bridge, BridgeTransfer, Burn, BurnEpoch, DemoPool, Evidence, Facts, FaucetPolicy, FaucetStatus, FaucetGrant, LabScenario, RegistryConfig, SchedulePreview, SpecMap } from './schemas.js'
import type { HashExchangePair, HashExchange as HashExchangeView, HubHashExchange, Cblock, Certificate, ExecutionSchedule, Hub, LabProtocol, LabRun, Member, MergedTransaction, NetSnapshot, Parameters, Proposer, Slot, Transaction, Window } from './schemas.js'
import type { ApiErrorBody } from './errors.js'
import type { LabReplay, ProtocolEvent } from './events.js'

export const FIXTURE_TIME = '2026-11-16T00:00:00.000Z'
function seedOf(seed: string | number) {
  if (typeof seed === 'number') return seed >>> 0
  let n = 0x811c9dc5
  for (let i = 0; i < seed.length; i++) n = Math.imul(n ^ seed.charCodeAt(i), 0x01000193) >>> 0
  return n
}
function sampleBytes(seed: string | number, length: number) {
  let state = seedOf(seed)
  return Uint8Array.from({ length }, (_, i) => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0
    return (state >>> 24) ^ ((i * 17) & 255)
  })
}
export const fixtureAddress = (seed: string | number) => toBase58(sampleBytes(seed, 32))
export const fixtureSignature = (seed: string | number) => toBase58(sampleBytes(seed, 64))
export const fixtureHash = (seed: string | number) => [...sampleBytes(seed, 32)].map(v => v.toString(16).padStart(2, '0')).join('')
/** Display bytes only: never a signed Solana transaction. */
export const fixtureTransaction = (label: string) => toBase64(new TextEncoder().encode(`FIXTURE_NOT_A_SOLANA_TRANSACTION:${label}`))
/** Explicit synthetic profile, not decisions for the real network. */
export const fixtureParameters: Parameters = {
  ...PROTOCOL_PARAMETERS, trackCapacityCu: 500000, fragmentExpirySlots: 32,
  alpha: 0.5, minimumHubMembers: 3, minimumHubStake: '1000000000',
}
function ecef(latitude: number, longitude: number) {
  const lat = latitude * Math.PI / 180, lon = longitude * Math.PI / 180
  const a = 6378137, e2 = 6.69437999014e-3
  const n = a / Math.sqrt(1 - e2 * Math.sin(lat) ** 2)
  return { x: Math.round(n * Math.cos(lat) * Math.cos(lon)), y: Math.round(n * Math.cos(lat) * Math.sin(lon)), z: Math.round(n * (1 - e2) * Math.sin(lat)) }
}
function makeHub(code: string, name: string, region: string, latitude: number, longitude: number, scale = 1): Hub {
  const stakes = [6000000000, 2000000000, 1200000000, 800000000].map(n => BigInt(Math.round(n * scale)))
  let cap = 0n
  for (let j = 0; j < 3; j++) {
    cap = stakes.slice(j).reduce((sum, n) => sum + n, 0n) / BigInt(3 - j)
    if (stakes[j]! <= cap) break
  }
  const members: Member[] = stakes.map((stake, i) => ({
    chain: 'acrux', identity: fixtureAddress(`${code}-validator-${i + 1}`), hubCode: code, stake: stake.toString(),
    proposerStake: (stake > cap ? cap : stake).toString(), capped: stake > cap,
    effectiveEpoch: 41, joinedEpoch: 40, pendingMove: null, pendingLeave: false, cooldownUntilEpoch: 41,
    validatorKind: ['FRA', 'NYC', 'TYO'].includes(code) ? 'genesis' : 'external', admittedAtEpoch: ['FRA', 'NYC', 'TYO'].includes(code) ? 41 : null,
  }))
  return { chain: 'acrux', code, name, region, latitude, longitude, ecef: ecef(latitude, longitude),
    registrant: fixtureAddress(`${code}-registrant`), depositLamports: '1000000000', members,
    stake: stakes.reduce((sum, n) => sum + n, 0n).toString(),
    proposerStake: members.reduce((sum, m) => sum + BigInt(m.proposerStake), 0n).toString(),
    proposerStakeCap: cap.toString(), status: 'active', needs: { members: 0, stake: '0' },
    createdEpoch: 40, lastActiveEpoch: 42, releasableEpoch: null, releasable: false,
    formedOutside: members.every(member => member.validatorKind === 'external'), activatedAtEpoch: 41 }
}
const frankfurt = makeHub('FRA', 'Frankfurt', 'Europe', 50.0379, 8.5622)
const newYork = makeHub('NYC', 'New York', 'North America', 40.6413, -73.7781, 0.75)
const tokyo = makeHub('TYO', 'Tokyo', 'Asia Pacific', 35.5494, 139.7798, 0.5)
const london = makeHub('LHR', 'London', 'Europe', 51.47, -0.4543, 0.1)
london.status = 'pending'; london.members = london.members.slice(2)
london.stake = '200000000'; london.proposerStake = '0'; london.proposerStakeCap = '0'
london.needs = { members: 1, stake: '1000000000' }; london.lastActiveEpoch = null; london.activatedAtEpoch = null
const singapore = makeHub('SIN', 'Singapore', 'Asia Pacific', 1.3644, 103.9915)
singapore.status = 'inactive'; singapore.members = []; singapore.stake = '0'; singapore.proposerStake = '0'; singapore.proposerStakeCap = '0'
singapore.needs = { members: 3, stake: '1000000000' }; singapore.lastActiveEpoch = 35; singapore.releasableEpoch = 40; singapore.releasable = true; singapore.activatedAtEpoch = undefined
export const hubs: Hub[] = [frankfurt, newYork, tokyo, london, singapore]
export const registryConfig: z.infer<typeof RegistryConfig> = {
  status: 'fixture', chain: 'solana', programId: null, cluster: 'localnet', depositLamports: null,
  registrationFeeBps: null, cooldownEpochs: null, inactivityEpochs: null,
  codeRules: { pattern: null, minLength: null, maxLength: null }, minimumMembers: null, minimumStake: null,
  currentEpoch: 42, missingConfiguration: ['REGISTRY_PROGRAM_ID', 'REGISTRY_DEPOSIT_LAMPORTS', 'REGISTRY_REGISTRATION_FEE_BPS', 'REGISTRY_COOLDOWN_EPOCHS', 'REGISTRY_INACTIVITY_EPOCHS', 'REGISTRY_CODE_PATTERN', 'REGISTRY_MIN_MEMBERS', 'REGISTRY_MIN_STAKE'],
}
function random(seed: string | number) {
  let state = seedOf(seed)
  return () => ((state = (Math.imul(state, 1664525) + 1013904223) >>> 0) / 4294967296)
}
function shuffle<T>(values: T[], rng: () => number) {
  const output = [...values]
  for (let i = output.length - 1; i > 0; i--) { const j = Math.floor(rng() * (i + 1)); [output[i], output[j]] = [output[j]!, output[i]!] }
  return output
}
function proposers(hub: Hub, seed: string | number): Proposer[] {
  const rng = random(seed), members = shuffle(hub.members, rng), cap = Number(hub.proposerStakeCap)
  let next = rng() * cap, cumulative = 0
  const chosen: Member[] = []
  for (const member of members) {
    cumulative += Number(member.proposerStake)
    if (next < cumulative && chosen.length < 3) { chosen.push(member); next += cap }
  }
  return shuffle(chosen, rng).map((member, i) => ({ index: i + 1, identity: member.identity, stake: member.stake, status: 'online', validatorKind: member.validatorKind }))
}
export function makeFixtureWindow(number: number): Window {
  const hub = hubs[number % 3]!
  return { number, hubCode: hub.code, firstSlot: number * 16, lastSlot: number * 16 + 15,
    consensusSlots: Array.from({ length: 4 }, (_, i) => number * 4 + i), proposers: proposers(hub, `window-${number}`) }
}
/** Algorithm 3 draw and systematic sampling; regional order is synthetic, pending SIMD-0675. */
export function makeSchedulePreview(epoch = 43): z.infer<typeof SchedulePreview> {
  const rng = random(`epoch-${epoch}`), active = hubs.filter(h => h.status === 'active'), totalWindows = 48
  const totalStake = active.reduce((sum, h) => sum + Number(h.proposerStake), 0)
  const shares = active.map(h => ({ hubCode: h.code, proposerStake: h.proposerStake,
    probability: 0.5 * Number(h.proposerStake) / totalStake + 0.5 / active.length, assignedWindows: 0 }))
  const drawn: Hub[] = []
  for (let i = 0; i < totalWindows; i++) {
    const value = rng(); let cumulative = 0
    const found = shares.findIndex(s => (cumulative += s.probability) > value)
    const index = found < 0 ? active.length - 1 : found
    drawn.push(active[index]!); shares[index]!.assignedWindows++
  }
  const order = ['Europe', 'North America', 'Asia Pacific']
  drawn.sort((a, b) => order.indexOf(a.region) - order.indexOf(b.region))
  const windows: Window[] = drawn.map((h, i) => ({ number: i, hubCode: h.code, firstSlot: i * 16, lastSlot: i * 16 + 15,
    consensusSlots: Array.from({ length: 4 }, (_, j) => i * 4 + j), proposers: proposers(h, `${epoch}-${i}`) }))
  let first = 0
  const regionalRuns: z.infer<typeof SchedulePreview>['regionalRuns'] = []
  for (const region of order) {
    const count = drawn.filter(h => h.region === region).length
    if (count) regionalRuns.push({ region, firstWindow: first, lastWindow: first + count - 1 })
    first += count
  }
  return { epoch, seed: `epoch-${epoch}`, source: 'fixture', algorithm: 3, alpha: 0.5, totalWindows, shares, regionalRuns, windows }
}
export const schedulePreview = makeSchedulePreview()
const pairFixtures = (members: Member[]): HashExchangePair[] => members.flatMap((left, index) => members.slice(index + 1).map(right => {
  const real = left.validatorKind === 'external' || right.validatorKind === 'external'
  return { members: [left.identity, right.identity].sort() as [string, string], kind: real ? 'real' as const : 'applied' as const,
    p50: real ? 720 : 6500, p99: real ? 1400 : 11000, count: 512 }
}))
const appliedSpread = { p50: 5800, p99: 14200, count: 4096 }
export const hashExchangeGenesisOnly: HashExchangeView = { measuredAt: FIXTURE_TIME, measurement: 'roundTrip', hubs: hubs.slice(0, 3).map(hub => ({
  hubCode: hub.code, pairs: pairFixtures(hub.members), commitSpreadUs: { applied: { ...appliedSpread, count: 1024 }, real: null },
})) }
const outsideFixtureIdentity = fixtureAddress('external-validator-FRA')
const outsideFixtureMembers: Member[] = frankfurt.members.map((member, index) => index === 3
  ? { ...member, identity: outsideFixtureIdentity, validatorKind: 'external', joinedEpoch: 41, effectiveEpoch: 42, admittedAtEpoch: 42 } : member)
export const hashExchangeWithOutside: HashExchangeView = { ...hashExchangeGenesisOnly, hubs: hashExchangeGenesisOnly.hubs.map(hub => hub.hubCode === 'FRA'
  ? { ...hub, pairs: pairFixtures(outsideFixtureMembers), commitSpreadUs: { applied: { ...appliedSpread, count: 768 }, real: { p50: 3400, p99: 6800, count: 256 } } } : hub) }
const hubExchange = (hub: HashExchangeView['hubs'][number]): HubHashExchange => ({ measuredAt: FIXTURE_TIME, measurement: 'roundTrip', pairs: hub.pairs, commitSpreadUs: hub.commitSpreadUs })
for (const hub of hubs.slice(0, 3)) hub.hashExchange = hubExchange(hashExchangeGenesisOnly.hubs.find(measured => measured.hubCode === hub.code)!)
export const hubHashExchangeGenesisOnly: Hub = { ...frankfurt, hashExchange: hubExchange(hashExchangeGenesisOnly.hubs[0]!) }
export const hubHashExchangeWithOutside: Hub = { ...frankfurt, members: outsideFixtureMembers, hashExchange: hubExchange(hashExchangeWithOutside.hubs[0]!) }
/** Four outside members meet the hub's membership and stake thresholds at epoch 43. */
export const outsideFormedHub: Hub = { ...makeHub('SIN', 'Singapore', 'Asia Pacific', 1.3644, 103.9915, 1.5),
  createdEpoch: 42, lastActiveEpoch: 43, formedOutside: true, activatedAtEpoch: 43 }
outsideFormedHub.members = outsideFormedHub.members.map(member => ({ ...member, validatorKind: 'external', joinedEpoch: 42, effectiveEpoch: 43, admittedAtEpoch: 43, cooldownUntilEpoch: 43 }))
outsideFormedHub.hashExchange = { measuredAt: FIXTURE_TIME, measurement: 'roundTrip', pairs: pairFixtures(outsideFormedHub.members),
  commitSpreadUs: { applied: null, real: { p50: 3400, p99: 6800, count: 256 } } }
const outsideFormedFirstSlot = 43 * 432000
export const outsideFormedWindow: Window = { number: outsideFormedFirstSlot / 16, hubCode: 'SIN', firstSlot: outsideFormedFirstSlot, lastSlot: outsideFormedFirstSlot + 15,
  consensusSlots: Array.from({ length: 4 }, (_, index) => outsideFormedFirstSlot / 4 + index), proposers: proposers(outsideFormedHub, 'outside-formed-SIN') }
export const outsideFormedFixture = { hub: outsideFormedHub, window: outsideFormedWindow }

export const facts: z.infer<typeof Facts> = {
  source: 'fixture', measuredAt: FIXTURE_TIME, sampleSize: 4096,
  hashExchange: hashExchangeGenesisOnly, commitSpreadByLinkUs: { applied: appliedSpread, real: null },
  slotPacingUs: { p50: 50000, p99: 50700 }, commitSpreadUs: { p50: 5800, p99: 14200 },
  attestationRelativeToBlockUs: { p50: -6200, p99: 13000 }, cblockFinalizationUs: { p50: 388000, p99: 571000 },
  handoverUs: {
    'FRA-FRA': { p50: 147099, p99: 153624, floorUs: 135000, count: 252 }, 'NYC-NYC': { p50: 147536, p99: 155239, floorUs: 135000, count: 214 },
    'TYO-TYO': { p50: 179892, p99: 188258, floorUs: 167000, count: 175 }, 'FRA-NYC': { p50: 183210, p99: 186194, floorUs: 170000, count: 7 },
    'NYC-TYO': { p50: 160092, p99: 162921, floorUs: 151000, count: 6 }, 'TYO-FRA': { p50: 167554, p99: 171325, floorUs: 158000, count: 7 },
  },
}
export const factsGenesisOnly = facts
export const factsWithOutside: z.infer<typeof Facts> = { ...facts, hashExchange: hashExchangeWithOutside,
  commitSpreadByLinkUs: { applied: { ...appliedSpread, count: 3840 }, real: { p50: 3400, p99: 6800, count: 256 } } }
export const hashExchangeFixtures = { genesisOnly: { facts: factsGenesisOnly, hub: hubHashExchangeGenesisOnly }, withOutside: { facts: factsWithOutside, hub: hubHashExchangeWithOutside } }
export const demoPool: z.infer<typeof DemoPool> = {
  address: fixtureAddress('demo-pool'), mints: [
    { address: fixtureAddress('demo-mint-a'), symbol: 'AXA', name: 'Acrux Demo A', decimals: 6 },
    { address: fixtureAddress('demo-mint-b'), symbol: 'AXB', name: 'Acrux Demo B', decimals: 6 },
  ], reserves: ['1000000000000', '2000000000000'], feeBps: 30, slot: 64,
}
export const faucetPolicy: z.infer<typeof FaucetPolicy> = { amountLamports: null, perKey: null, perIp: null, periodSeconds: null,
  missingConfiguration: ['FAUCET_GRANT_LAMPORTS', 'FAUCET_PER_KEY_LIMIT', 'FAUCET_PER_IP_LIMIT', 'FAUCET_WINDOW_SECONDS'] }
export function makeFixtureBalance(address = fixtureAddress('sender'), funded = true, slot = 64): z.infer<typeof Balance> {
  return { address, lamports: funded ? '5000000000' : '0', reserveLamports: '1000000', spendableLamports: funded ? '4999000000' : '0', slot }
}
export function makeFaucetStatus(address = fixtureAddress('sender'), limited = false): z.infer<typeof FaucetStatus> {
  return { address, policy: faucetPolicy, remainingForKey: limited ? 0 : null, remainingForIp: limited ? 0 : null,
    resetsAt: limited ? '2026-11-17T00:00:00.000Z' : null, eligible: false, availableLamports: null }
}
export const faucetStatus = makeFaucetStatus
export function faucetGrant(address = fixtureAddress('sender')): z.infer<typeof FaucetGrant> {
  return { id: 'fixture-grant', address, amountLamports: '1000000000', signature: fixtureSignature(`grant-${address}`),
    balance: makeFixtureBalance(address), grantedAt: FIXTURE_TIME }
}
const txLengths = [80000, 60000, 90000, 45000, 50000, 30000, 80000, 55000, 35000]
const txAccounts = [['A', 'B'], ['A'], ['C', 'D'], ['E'], ['C', 'F'], ['G'], ['B', 'E'], ['H'], ['D', 'G']]
function mergedTransactions(slot: number, sources: number[]): MergedTransaction[] {
  return txLengths.map((computeLimit, i) => ({
    id: fixtureHash(`tx-${slot}-${i}`), feePayer: fixtureAddress(`payer-${i}`), priorityFeePerCu: String(12000 - i * 800),
    computeLimit, writableAccounts: txAccounts[i]!.map(a => fixtureAddress(`account-${a}`)), targetSlot: slot,
    kind: i % 3 === 0 ? 'fragments' : 'plain', sourceProposers: [...sources], n: sources.length,
    t: i % 3 === 0 ? 2 : 1, fragmentIndices: i % 3 === 0 ? sources.map(s => s - 1) : [],
    position: i, selected: true, exclusion: null,
  }))
}
function fixtureSchedule(txs: MergedTransaction[]): ExecutionSchedule {
  type Space = { start: number; end: number; track: number }
  let spaces: Space[] = [1, 2, 3, 4].map(track => ({ start: 0, end: 500000, track }))
  const placements: ExecutionSchedule['placements'] = []
  for (const tx of txs) {
    const examined: Space[] = []
    let placed = false
    while (!placed && spaces.length) {
      spaces.sort((a, b) => a.start - b.start || a.track - b.track || a.end - b.end)
      const space = spaces.shift()!
      const ends = [0, ...placements.map(p => p.startCu + p.lengthCu)].sort((a, b) => a - b)
      for (const end of ends) {
        const start = Math.max(space.start, end)
        if (start + tx.computeLimit > space.end) continue
        if (placements.some(p => p.startCu < start + tx.computeLimit && p.startCu + p.lengthCu > start &&
          txs.find(t => t.id === p.txId)!.writableAccounts.some(a => tx.writableAccounts.includes(a)))) continue
        placements.push({ track: space.track, startCu: start, lengthCu: tx.computeLimit, txId: tx.id })
        if (space.start < start) examined.push({ ...space, end: start })
        if (start + tx.computeLimit < space.end) examined.push({ ...space, start: start + tx.computeLimit })
        placed = true; break
      }
      if (!placed) examined.push(space)
    }
    spaces = spaces.concat(examined)
    tx.selected = placed; tx.exclusion = placed ? null : 'capacity'
  }
  return { algorithm: 4, tracks: 4, capacityCu: 500000, placements,
    gaps: spaces.sort((a, b) => a.track - b.track || a.start - b.start).map(s => ({ track: s.track, startCu: s.start, lengthCu: s.end - s.start })) }
}
export type FixtureSlotOptions = { crashedProposer?: number; externalProposer?: number; skipReason?: NonNullable<Slot['skip']>['reason'] }
export function makeFixtureSlot(slot = 64, status: Slot['status'] = 'finalized', options: FixtureSlotOptions = {}): Slot {
  const window = makeFixtureWindow(Math.floor(slot / 16)), consensusSlot = Math.floor(slot / 4)
  if (options.externalProposer) {
    const proposer = window.proposers[options.externalProposer - 1]!
    proposer.identity = fixtureAddress(`external-validator-${window.hubCode}`)
    proposer.validatorKind = 'external'
  }
  const skipReason = options.skipReason ?? 'disagreeing_vectors', crashed = options.crashedProposer
  if (crashed) window.proposers[crashed - 1]!.status = 'crashed'
  if (status === 'skipped' && !crashed) window.proposers[2]!.status = 'late'
  const sources = crashed ? [1, 2, 3].filter(n => n !== crashed) : [1, 2, 3]
  const hashes = [1, 2, 3].map(p => crashed === p ? null : fixtureHash(`block-${slot}-${p}`))
  const validators = hubs.slice(0, 3).flatMap(h => h.members.map(m => m.identity))
  const sends = [1, 2, 3].map(p => 50000 + (p - 1) * 2200 + (status === 'skipped' && p === 3 ? 28000 : 0))
  const arrivalTime = (sender: number, receiver: number) => sends[sender - 1]! + 1400 + receiver * 600
  const starts = [1, 2, 3].map(receiver => Math.min(sends[receiver - 1]!, ...sources.filter(s => s !== receiver).map(s => arrivalTime(s, receiver))))
  const blocks: Slot['blocks'] = window.proposers.map(p => {
    if (p.index === crashed) return { proposer: p.index, identity: p.identity, validatorKind: p.validatorKind, status: 'missing', bytes: 0, slices: [], commitment: null, attestation: null, parent: null }
    const sentUs = sends[p.index - 1]!, startUs = starts[p.index - 1]!
    const arrivals = sources.filter(q => q !== p.index).map(receiver => {
      const arrivedUs = arrivalTime(p.index, receiver), receiverStartUs = starts[receiver - 1]!
      return { receiver, startUs: receiverStartUs, arrivedUs, relativeToStartUs: arrivedUs - receiverStartUs, deadlineUs: 20000 as const, onTime: arrivedUs <= receiverStartUs + 20000 }
    })
    const commitment = { slot, proposer: p.index, hash: hashes[p.index - 1]!, keyHash: fixtureHash(`key-${slot}-${p.index}`), signature: fixtureSignature(`commit-${slot}-${p.index}`), sentUs, arrivals }
    const pi = hashes.map((hash, index) => index + 1 === p.index || (hash !== null && arrivalTime(index + 1, p.index) <= startUs + 20000) ? hash : null)
    const allArrivals = sources.filter(q => q !== p.index).map(q => arrivalTime(q, p.index))
    const attestedUs = Math.max(sentUs, crashed ? startUs + 20000 : Math.min(startUs + 20000, Math.max(sentUs, ...allArrivals)))
    const attestation = { slot, proposer: p.index, pi, key: toBase64(sampleBytes(`key-${slot}-${p.index}`, 32)), signature: fixtureSignature(`attestation-${slot}-${p.index}`),
      sentUs: attestedUs, validatorArrivals: validators.map((identity, i) => ({ identity, arrivedUs: attestedUs + 12000 + (i % 4) * 1500 + Math.floor(i / 4) * 58000 })) }
    return { proposer: p.index, identity: p.identity, validatorKind: p.validatorKind, status: status === 'pending' ? 'streaming' : status === 'skipped' ? 'dropped' : 'accepted', bytes: 115200,
      slices: (status === 'pending' ? [0] : [0, 1, 2]).map(index => ({ index, shredCount: 64 as const, rebuildShreds: 32 as const, bytes: 38400, sentUs: sentUs - (2 - index) * 16000,
        arrivedUs: sentUs - (2 - index) * 16000 + 170000, merkleRoot: fixtureHash(`slice-${slot}-${p.index}-${index}`) })),
      commitment: status === 'pending' ? null : commitment, attestation: status === 'pending' ? null : attestation,
      parent: slot % 16 === 0 ? { consensusSlot: Math.max(0, consensusSlot - 1), hash: fixtureHash(`cblock-${Math.max(0, consensusSlot - 1)}`) } : null }
  })
  const attestations = blocks.flatMap(b => b.attestation ? [b.attestation] : [])
  if (status === 'skipped' && skipReason === 'missing_attestation') {
    blocks[2]!.attestation = null
    attestations.splice(0, attestations.length, ...blocks.flatMap(b => b.attestation ? [b.attestation] : []))
    for (const attestation of attestations) attestation.pi = blocks.map(b => b.commitment!.hash)
  }
  if (status === 'skipped' && skipReason === 'majority_lost') {
    for (const block of blocks.slice(1)) { block.status = 'missing'; block.slices = []; block.bytes = 0; block.commitment = null; block.attestation = null }
    attestations.splice(1)
    attestations[0]!.pi = [blocks[0]!.commitment!.hash, null, null]
    window.proposers[1]!.status = 'crashed'; window.proposers[2]!.status = 'crashed'
  }
  const mergedOrder = status === 'pending' || status === 'skipped' ? [] : mergedTransactions(slot, sources)
  const schedule = status === 'pending' || status === 'skipped' ? null : fixtureSchedule(mergedOrder)
  const stateHash = status === 'pending' || status === 'skipped' ? null : fixtureHash(`state-${slot}`)
  const executedUs = Math.max(...sources.map(p => sends[p - 1]!)) + 180000
  const timeoutUs = 750000 + ((consensusSlot % 4) + 1) * 200000 + 50000
  const firstVoteUs = status === 'skipped' ? timeoutUs - (consensusSlot % 4) * 200000 : crashed && consensusSlot % 4 === 0 ? 570000 : 400000
  const finalizedUs = firstVoteUs + 30000
  const hash = fixtureHash(`cblock-${consensusSlot}`), signers = validators.slice(0, 10)
  const certificate: Certificate = { consensusSlot, kind: status === 'skipped' ? 'skip' : 'fast_finalization', stakeShare: 10 / 12, threshold: status === 'skipped' ? 0.6 : 0.8,
    hash: status === 'skipped' ? null : hash, signers, atUs: finalizedUs, linkedConsensusSlot: null }
  const cblock: Cblock = { consensusSlot, slots: Array.from({ length: 4 }, (_, i) => consensusSlot * 4 + i),
    parent: consensusSlot ? { consensusSlot: consensusSlot - 1, hash: fixtureHash(`cblock-${consensusSlot - 1}`) } : null,
    hash: status === 'pending' || status === 'skipped' ? null : hash, stateHash: status === 'pending' || status === 'skipped' ? null : fixtureHash(`state-${consensusSlot * 4 + 3}`),
    votes: status === 'pending' || status === 'executed' ? [] : validators.map((validator, i) => ({ consensusSlot, validator, kind: status === 'skipped' ? 'skip' : 'notar', stakeShare: 1 / 12, hash: status === 'skipped' ? null : hash, atUs: firstVoteUs + i * 1800 })),
    certificates: status === 'finalized' ? [{ ...certificate, kind: 'notarization', stakeShare: 8 / 12, signers: validators.slice(0, 8), threshold: 0.6, atUs: firstVoteUs + 14000 }, certificate] : status === 'skipped' ? [certificate] : [],
    status: status === 'pending' || status === 'executed' ? 'pending' : status === 'skipped' ? 'skipped' : 'finalized', finalizedUs: status === 'finalized' ? finalizedUs : null }
  return { slot, wallTime: new Date(Date.parse(FIXTURE_TIME) + slot * 50).toISOString(), window, consensusSlot, status, blocks, attestations,
    agreedSet: status === 'pending' || status === 'skipped' ? null : sources,
    skip: status === 'skipped' ? { reason: skipReason, detail: skipReason === 'disagreeing_vectors' ? 'Proposer 3 arrived after 2δlocal. The attestations disagree; the cblock and remaining window are skipped.' : `Fixture ${skipReason.replaceAll('_', ' ')}.`,
      disagreeingVectors: ['missing_attestation', 'majority_lost'].includes(skipReason) ? [] : attestations, affectedSlots: Array.from({ length: window.lastSlot - consensusSlot * 4 + 1 }, (_, i) => consensusSlot * 4 + i), atUs: timeoutUs - (slot % 16) * 50000 } : null,
    mergedOrder, schedule, stateHash, xblockHash: stateHash ? fixtureHash(`xblock-${slot}`) : null, cblock,
    executedUs: stateHash ? executedUs : null, attestationDeadlineUs: 570000,
    consensusTimeoutUs: 750000 + ((consensusSlot % 4) + 1) * 200000 + 50000 }
}
export const slotsByState = {
  pending: makeFixtureSlot(64, 'pending'), executed: makeFixtureSlot(64, 'executed'),
  finalized: makeFixtureSlot(64, 'finalized'), skipped: makeFixtureSlot(70, 'skipped'),
  crash: makeFixtureSlot(80, 'finalized', { crashedProposer: 3 }),
  disagreeing_vectors: makeFixtureSlot(70, 'skipped'),
  missing_attestation: makeFixtureSlot(70, 'skipped', { skipReason: 'missing_attestation' }),
  equivocation: makeFixtureSlot(70, 'skipped', { skipReason: 'equivocation' }),
  majority_lost: makeFixtureSlot(70, 'skipped', { skipReason: 'majority_lost' }),
  external: makeFixtureSlot(96, 'finalized', { externalProposer: 1 }),
}
export function makeFixtureTransaction(state: Transaction['state'] = 'finalized', slot = 64, id = fixtureHash(`tx-${slot}-0`)): Transaction {
  const target = makeFixtureSlot(slot), first = target.mergedOrder[0]!
  const executed = state === 'executed' || state === 'finalized' || state === 'failed'
  const count = state === 'unknown' || state === 'sent' ? 0 : state === 'partly_included' || state === 'expired' ? 1 : 3
  return { header: { id, feePayer: first.feePayer, priorityFeePerCu: first.priorityFeePerCu, computeLimit: first.computeLimit, writableAccounts: first.writableAccounts, targetSlot: slot },
    state, kind: 'fragments', n: 3, t: 2, receivers: target.window.proposers,
    fragments: Array.from({ length: count }, (_, i) => ({ index: i, receiver: i + 1, slot, proposer: i + 1, includedUs: 228000 + i * 1000 })), includedCount: count,
    readableSlot: executed ? slot : null, executedSlot: executed ? slot : null, mergedPosition: executed ? 0 : null,
    result: state === 'failed' ? 'error' : executed ? 'success' : null,
    logs: executed ? state === 'failed' ? ['Program log: transfer', 'Program failed: insufficient spendable balance; reserve preserved.'] : ['Program log: demo transfer', 'Program consumed 42100 compute units', 'Program success'] : [],
    failure: state === 'failed' ? 'Insufficient spendable balance above the fee-payer reserve.' : state === 'expired' ? 'Only one of two required fragments arrived before expiry.' : null,
    expiresAfterSlot: slot + 32, finalization: state === 'finalized' ? target.cblock.certificates.find(c => c.kind === 'fast_finalization')! : null }
}
export const transactionsByState = Object.fromEntries((['unknown', 'sent', 'partly_included', 'executed', 'finalized', 'failed', 'expired'] as const)
  .map(state => [state, makeFixtureTransaction(state)])) as Record<Transaction['state'], Transaction>
/** A finalized transaction carried by the accepted external block in the same slot. */
export const externalBlockSlot = slotsByState.external
export const externalBlockTransaction: Transaction = { ...makeFixtureTransaction('finalized', externalBlockSlot.slot), receivers: externalBlockSlot.window.proposers }
export const externalBlockFixture = { slot: externalBlockSlot, transaction: externalBlockTransaction }
export const scenarios: z.infer<typeof LabScenario>[] = [
  { id: 'last-look', title: 'Last look', description: 'A proposer delays its commitment until the others reveal their keys.', seed: 'acrux-last-look-1', parameters: { attacker: 3, commitmentDelayUs: 28000 }, lemma: 'Lemma 2.3 — Proposal hiding', expectedOutcome: 'The late proposal is dropped and disagreeing vectors produce a skip certificate.' },
  { id: 'censorship', title: 'Censorship', description: 'Two proposers omit a transaction carried by the third.', seed: 'acrux-censorship-1', parameters: { censoringProposers: '1,2', honestProposer: 3 }, lemma: 'Lemma 2.2 — Selective-censorship resistance', expectedOutcome: 'The honest proposer’s transaction enters the agreed xblock and finalizes.' },
  { id: 'crash', title: 'Crash from window start', description: 'The third proposer sends no messages for the entire window.', seed: 'acrux-crash-1', parameters: { crashedProposer: 3, crashedAtWindowStart: true }, lemma: 'Theorem 2.1 — NewCon correctness', expectedOutcome: 'The window continues with P = {1,2}; the first vote waits for the attestation deadline.' },
  { id: 'equivocation', title: 'Equivocation', description: 'A proposer signs conflicting commitments for the same slot.', seed: 'acrux-equivocation-1', parameters: { attacker: 3, conflictingCommitments: 2 }, lemma: 'Lemma 2.4 — Accountable equivocation', expectedOutcome: 'The two signed commitments form a slashing proof and the cblock is skipped.' },
  { id: 'split-commitments', title: 'Split commitments', description: 'A proposer delivers its commitment to only one co-proposer.', seed: 'acrux-split-commitments-1', parameters: { attacker: 3, receiver: 1 }, lemma: 'Lemma 2.2 — Selective-censorship resistance', expectedOutcome: 'Different attestation vectors prevent agreement and yield a skip certificate.' },
  { id: 'private-front-run', title: 'Private front-run', description: 'One receiver attempts to read and front-run a same-slot 2-of-3 swap.', seed: 'acrux-private-front-run-1', parameters: { attacker: 1, n: 3, t: 2, simultaneousTargetSlot: true }, lemma: 'Section 3.2 — Threshold transactions; Lemma 2.3', expectedOutcome: 'One fragment is unreadable; the honest same-slot fragments reconstruct the swap after commitment.' },
]
function scenarioEvidence(scenario: z.infer<typeof LabScenario>, slot: Slot): z.infer<typeof Evidence> {
  const kind = ({ 'last-look': 'late_block_dropped', censorship: 'transaction_included', crash: 'two_proposers_continue', equivocation: 'slashing_proof', 'split-commitments': 'skip_certificate', 'private-front-run': 'unreadable_fragment' } as const)[scenario.id]
  return { kind, slot: slot.slot, proposer: scenario.id === 'censorship' ? 3 : scenario.id === 'private-front-run' ? 1 : 3,
    txId: scenario.id === 'censorship' || scenario.id === 'private-front-run' ? fixtureHash(`tx-${slot.slot}-0`) : null,
    description: scenario.expectedOutcome, hashes: scenario.id === 'equivocation' ? [slot.blocks[2]!.commitment!.hash, fixtureHash('conflict-two')] : [],
    signatures: scenario.id === 'equivocation' ? [slot.blocks[2]!.commitment!.signature, fixtureSignature('conflict-two')] : [],
    observedFragments: scenario.id === 'private-front-run' ? 1 : null, threshold: scenario.id === 'private-front-run' ? 2 : null }
}
/** The window before the attack, the attacker's window 4 and the one after; a skip in window 4 when `skipped`. */
function labYardstick(skipped: boolean): NonNullable<LabRun['yardstick']> {
  const cblocks: NonNullable<LabRun['yardstick']>['cblocks'] = Array.from({ length: 12 }, (_, at) => {
    const cblock = 12 + at, window = Math.floor(cblock / 4), skip = skipped && cblock >= 16 && cblock < 20
    return skip
      ? { cblock, window, status: 'skipped', skipped: true, votes: { skip: 12 } as Record<string, number>, certificates: ['skip'], finalizedUs: null }
      : { cblock, window, status: 'finalized', skipped: false, votes: { notar: 12, finalize: 12 } as Record<string, number>, certificates: ['notarization', 'finalization'], finalizedUs: 412_000 + at * 1_500 }
  })
  return { attackerWindow: 4, slotsSkipped: skipped ? 16 : 0, cblocks }
}
export const labRuns: LabRun[] = scenarios.map(scenario => {
  const skipped = ['last-look', 'equivocation', 'split-commitments'].includes(scenario.id)
  const slot = makeFixtureSlot(64, skipped ? 'skipped' : 'finalized', { crashedProposer: scenario.id === 'crash' ? 3 : undefined, skipReason: scenario.id === 'equivocation' ? 'equivocation' : 'disagreeing_vectors' })
  return { id: `run-${scenario.id}`, scenario, seed: scenario.seed, protocol: 'today', status: 'done', createdAt: FIXTURE_TIME,
    startedAt: FIXTURE_TIME, endedAt: '2026-11-16T00:00:02.000Z', outcome: scenario.expectedOutcome,
    evidence: [scenarioEvidence(scenario, slot)], yardstick: labYardstick(skipped), failure: null }
})
export const labRunsByState: Record<LabRun['status'], LabRun> = {
  queued: { ...labRuns[0]!, id: 'run-queued', status: 'queued', startedAt: null, endedAt: null, outcome: null, evidence: [], yardstick: null },
  running: { ...labRuns[0]!, id: 'run-running', status: 'running', endedAt: null, outcome: null, evidence: [], yardstick: null },
  done: labRuns[0]!, failed_to_start: { ...labRuns[0]!, id: 'run-failed', status: 'failed_to_start', startedAt: null, endedAt: FIXTURE_TIME, outcome: null, evidence: [], yardstick: null, failure: 'The fixture sandbox could not acquire a runner.' },
}
/** Local agreement continues the split-commitment window; the lanes branch is not built yet. */
export const windowContinuedEvidence: Evidence = { kind: 'window_continued', slot: 64, attackSlot: 64, proposer: 3,
  txId: null, description: 'The split commitment was resolved by local agreement; the window continued and finalized.',
  hashes: [], signatures: [], observedFragments: null, threshold: null, agreedSet: [1, 2, 3], finalized: true }
const splitCommitmentRun = labRuns.find(run => run.scenario.id === 'split-commitments')!
export const labRunsByProtocol: Record<LabProtocol, LabRun> = {
  today: splitCommitmentRun,
  agreement: { ...splitCommitmentRun, id: 'run-split-commitments-agreement', protocol: 'agreement',
    outcome: windowContinuedEvidence.description, evidence: [windowContinuedEvidence], yardstick: labYardstick(false) },
  lanes: { ...splitCommitmentRun, id: 'run-split-commitments-lanes', protocol: 'lanes', status: 'failed_to_start',
    startedAt: null, outcome: null, evidence: [], yardstick: null, failure: 'The lanes protocol is not built yet.' },
}
/** Every envelope keeps its own anchor; sequence orders a replay across overlapping slots. */
export function makeLabReplay(run: LabRun): LabReplay {
  const continues = run.protocol === 'agreement' && run.scenario.id === 'split-commitments'
  const skipped = !continues && ['last-look', 'equivocation', 'split-commitments'].includes(run.scenario.id)
  const crash = run.scenario.id === 'crash'
  const count = crash || continues ? 16 : 4
  const slots = Array.from({ length: count }, (_, i) => makeFixtureSlot(64 + i, skipped ? 'skipped' : 'finalized', {
    crashedProposer: crash ? 3 : undefined,
    skipReason: run.scenario.id === 'equivocation' ? 'equivocation' : 'disagreeing_vectors',
  }))
  const timeline: { event: ProtocolEvent; offset: number }[] = []
  function add<T extends ProtocolEvent['type']>(type: T, data: Extract<ProtocolEvent, { type: T }>['data'], atUs: number, slot: number | null = 64, anchor: ProtocolEvent['anchor'] = 'slot') {
    const anchorStart = anchor === 'slot' ? ((slot ?? 64) - 64) * 50000 : anchor === 'consensus' ? (Math.floor((slot ?? 64) / 4) * 4 - 64) * 50000 : 0
    timeline.push({ event: { version: 1, sequence: timeline.length, streamId: `lab-${run.id}`, slot, anchor, atUs, type, data } as ProtocolEvent, offset: anchorStart + atUs })
  }
  add('lab.updated', { ...run, status: 'running', endedAt: null, outcome: null, evidence: [], yardstick: null }, 0, null, 'stream')
  add('window.started', slots[0]!.window, 0, null, 'window')
  for (const slot of slots) {
    if (run.scenario.id === 'censorship') { slot.mergedOrder[0]!.sourceProposers = [3]; slot.mergedOrder[0]!.kind = 'plain'; slot.mergedOrder[0]!.n = 3; slot.mergedOrder[0]!.t = 1; slot.mergedOrder[0]!.fragmentIndices = [] }
    if (skipped && (run.scenario.id === 'equivocation' || run.scenario.id === 'split-commitments')) {
      const healthy = makeFixtureSlot(slot.slot)
      slot.blocks = healthy.blocks.map(block => ({ ...block, status: 'dropped' }))
      slot.attestations = healthy.attestations
      slot.window = healthy.window
      slot.skip!.disagreeingVectors = slot.attestations
    }
    if (run.scenario.id === 'split-commitments' && (!continues || slot.slot === 64)) {
      const third = slot.blocks[2]!
      third.commitment!.sentUs = 54400
      third.commitment!.arrivals = third.commitment!.arrivals.filter(a => a.receiver === 1).map(a => ({ ...a, arrivedUs: 56400, relativeToStartUs: 6400, onTime: true }))
      slot.attestations[0]!.pi[2] = third.commitment!.hash
      slot.attestations[1]!.pi[2] = null
      slot.attestations[2]!.pi[2] = third.commitment!.hash
      if (slot.skip) slot.skip.detail = 'Proposer 3 sent its commitment only to proposer 1; vectors disagree.'
    }
    if (run.scenario.id === 'equivocation') {
      slot.attestations[2]!.pi[2] = fixtureHash('conflict-two')
      slot.skip!.detail = 'Proposer 3 signed conflicting commitments for one slot.'
    }
    if (skipped && slot.slot > 64) {
      slot.skip!.reason = 'window_skipped'
      slot.skip!.detail = 'A skip in the first cblock also skips the remaining window.'
      slot.skip!.atUs = Math.max(0, slots[0]!.skip!.atUs - (slot.slot - 64) * 50000)
    }
    const pending = makeFixtureSlot(slot.slot, 'pending', { crashedProposer: crash ? 3 : undefined })
    pending.blocks.forEach(b => { b.slices = []; b.bytes = 0 })
    add('slot.started', pending, 0, slot.slot)
    for (const block of slot.blocks) {
      for (const slice of block.slices) add('block.slice', { proposer: block.proposer, slice }, slice.sentUs, slot.slot)
      if (block.commitment) {
        add('commitment.sent', block.commitment, block.commitment.sentUs, slot.slot)
        for (const arrival of block.commitment.arrivals) add('commitment.arrived', { proposer: block.proposer, hash: block.commitment.hash, arrival }, arrival.arrivedUs, slot.slot)
        if (run.scenario.id === 'equivocation' && block.proposer === 3) {
          add('commitment.sent', { ...block.commitment, hash: fixtureHash('conflict-two'), signature: fixtureSignature('conflict-two') }, block.commitment.sentUs + 500, slot.slot)
        }
      }
      if (block.attestation) {
        add('attestation.sent', block.attestation, block.attestation.sentUs, slot.slot)
        for (const arrival of block.attestation.validatorArrivals) add('attestation.arrived', { attestation: block.attestation, validator: arrival.identity, arrivedUs: arrival.arrivedUs }, arrival.arrivedUs, slot.slot)
      }
    }
    if (slot.skip) add('slot.skipped', slot.skip, slot.skip.atUs, slot.slot)
    else {
      const done = slot.executedUs!
      add('slot.agreed', { agreedSet: slot.agreedSet! }, done - 5000, slot.slot)
      for (const block of slot.blocks.filter(b => b.status === 'accepted')) add('block.decrypted', { proposer: block.proposer, transactionIds: slot.mergedOrder.filter(tx => tx.sourceProposers.includes(block.proposer)).map(tx => tx.id) }, done - 4000, slot.slot)
      add('slot.merged', { transactions: slot.mergedOrder }, done - 3000, slot.slot)
      add('slot.scheduled', slot.schedule!, done - 2000, slot.slot)
      add('slot.executed', { ...slot, status: 'executed', cblock: { ...slot.cblock, status: 'pending', certificates: [], votes: [], finalizedUs: null } }, done, slot.slot)
      add('transaction.updated', makeFixtureTransaction('executed', slot.slot), done, slot.slot)
    }
    if (slot.slot % 4 === 3) {
      const cblock = slot.cblock
      if (!slot.skip) {
        const first = Math.floor(slot.slot / 4) * 4
        const assembledUs = Math.max(...slots.filter(s => s.consensusSlot === slot.consensusSlot).map(s => (s.slot - first) * 50000 + (s.executedUs ?? 0))) + 1000
        add('cblock.assembled', { ...cblock, status: 'pending', votes: [], certificates: [], finalizedUs: null }, assembledUs, slot.slot, 'consensus')
      }
      for (const vote of cblock.votes) add('cblock.vote', vote, vote.atUs, slot.slot, 'consensus')
      for (const certificate of cblock.certificates) add('cblock.certificate', certificate, certificate.atUs, slot.slot, 'consensus')
      if (!slot.skip) {
        add('cblock.finalized', cblock, cblock.finalizedUs!, slot.slot, 'consensus')
        for (let s = slot.slot - 3; s <= slot.slot; s++) add('transaction.updated', makeFixtureTransaction('finalized', s), cblock.finalizedUs!, s, 'consensus')
      }
    }
  }
  const finish = Math.max(crash ? 1150000 : 650000, ...timeline.map(item => item.offset)) + 50000
  if (run.scenario.id === 'private-front-run') {
    const partial = makeFixtureTransaction('partly_included')
    add('transaction.updated', partial, 229000, 64)
  }
  for (const evidence of run.evidence) add('lab.evidence', evidence, finish, evidence.slot, 'stream')
  add('lab.updated', run, finish + 50000, null, 'stream')
  timeline.sort((a, b) => a.offset - b.offset || a.event.sequence - b.event.sequence)
  timeline.forEach(({ event }, i) => { event.sequence = i })
  return { run, events: timeline.map(({ event }) => event) }
}
/** Wall-clock offset from the replay's first slot; use this to pace events with different anchors. */
export function replayOffsetUs(event: ProtocolEvent, firstSlot = 64): number {
  if (event.anchor === 'slot') return ((event.slot ?? firstSlot) - firstSlot) * 50000 + event.atUs
  if (event.anchor === 'consensus') return (Math.floor((event.slot ?? firstSlot) / 4) * 4 - firstSlot) * 50000 + event.atUs
  return event.atUs
}
export const labReplays = Object.fromEntries([...labRuns, labRunsByProtocol.agreement].map(run => [run.id, makeLabReplay(run)]))
export const specMap: z.infer<typeof SpecMap> = {
  paper: 'New Constellation v1.0 — Kniep and Wattenhofer, 2026-11-14', parameters: fixtureParameters,
  entries: [
    { reference: 'Algorithm 1', lines: '1–32', file: 'node/crates/proposer/src/lib.rs', function: 'run_window', scenario: 'last-look', status: 'planned' },
    { reference: 'Algorithm 2', lines: '1–59', file: 'node/crates/validator/src/lib.rs', function: 'try_execute_and_vote', scenario: 'split-commitments', status: 'planned' },
    { reference: 'Algorithm 3', lines: '1–15', file: 'contract/src/fixtures.ts', function: 'makeSchedulePreview', scenario: null, status: 'fixture' },
    { reference: 'Algorithm 4', lines: '1–50', file: 'fixtures/src/selection.ts', function: 'selectTransactions', scenario: 'censorship', status: 'fixture' },
    { reference: 'Theorem 2.1', lines: null, file: 'node/crates/validator/src/lib.rs', function: 'try_vote', scenario: 'crash', status: 'planned' },
    { reference: 'Lemma 2.2', lines: null, file: 'node/crates/validator/src/lib.rs', function: 'agreed', scenario: 'censorship', status: 'planned' },
    { reference: 'Lemma 2.3', lines: null, file: 'node/crates/proposer/src/lib.rs', function: 'attest', scenario: 'last-look', status: 'planned' },
    { reference: 'Lemma 2.4', lines: null, file: 'node/crates/validator/src/lib.rs', function: 'detect_equivocation', scenario: 'equivocation', status: 'planned' },
    { reference: 'Section 3.2', lines: null, file: 'node/crates/threshold/src/lib.rs', function: 'accept_fragment', scenario: 'private-front-run', status: 'planned' },
  ], decisions: [
    { topic: 'Block encryption', decision: 'ChaCha20-Poly1305 with H(K) bound into the commitment.', status: 'decided' },
    { topic: 'Threshold encoding', decision: 'Reed-Solomon ciphertext fragments and Shamir key shares over GF(256).', status: 'decided' },
    { topic: 'Track capacity and fragment expiry', decision: '500,000 CU and 32 slots in synthetic fixtures only; real-network values require configuration.', status: 'fixture_only' },
    { topic: 'Regional ordering', decision: 'Fixture preview groups seeded hub draws into three regions; exact SIMD-0675 ordering remains for the node round.', status: 'fixture_only' },
    { topic: 'Registry and faucet', decision: 'Deposit, registration fee, cooldown, inactivity, code rules and faucet limits must be configured.', status: 'configuration_required' },
  ],
}
const specialMessages: Record<string, string> = { FAUCET_RETIRED: FAUCET_RETIRED_MESSAGE, NOT_FOUND: 'The requested resource was not found.', RATE_LIMITED: 'Too many requests. Try again shortly.', FAUCET_LIMIT: 'The faucet limit for this key or IP has been reached.', FAUCET_EMPTY: 'The faucet cannot pay this grant.', GRANT_FAILED: 'The grant transaction failed on Acrux Net.', INVALID_INPUT: 'The request contains invalid input.', VALIDATION_ERROR: 'The request contains invalid input.', NET_STALLED: 'No new slot has arrived for one second.', NET_UNAVAILABLE: 'Acrux Net is unavailable.', PROGRAM_UNAVAILABLE: 'Waiting for the registry program.', CONFIGURATION_REQUIRED: 'Required configuration has not been set.', ORIGIN_NOT_ALLOWED: 'This origin is not allowed.', TRANSACTION_EXPIRED: 'The transaction fragments expired before reaching the threshold.' }
export const failureFixtures: Record<keyof typeof errors.status, ApiErrorBody> = Object.fromEntries(Object.keys(errors.status).map(code => [code, {
  error: { code, message: specialMessages[code] ?? `Fixture failure: ${code.toLowerCase().replaceAll('_', ' ')}.`, retryable: ['RATE_LIMITED', 'FAUCET_LIMIT', 'NET_STALLED', 'NET_UNAVAILABLE', 'PROGRAM_UNAVAILABLE', 'UPSTREAM_UNAVAILABLE'].includes(code), stage: code === 'INVALID_INPUT' || code === 'VALIDATION_ERROR' ? 'validation' : 'read',
    ...(['RATE_LIMITED', 'FAUCET_LIMIT'].includes(code) ? { retryAfterSeconds: 60 } : {}),
    ...(['INVALID_INPUT', 'VALIDATION_ERROR'].includes(code) ? { fieldErrors: { address: ['Must be a base58 address of 32 bytes.'] } } : {}),
  },
}])) as Record<keyof typeof errors.status, ApiErrorBody>
export const fixtureErrors = failureFixtures
export function makeNetSnapshot(status: NetSnapshot['status'] = 'live'): NetSnapshot {
  return { streamId: 'fixture-net', cursor: 0, status, currentWindow: makeFixtureWindow(4),
    recentSlots: Array.from({ length: 64 }, (_, i) => makeFixtureSlot(i + 1)),
    upcomingWindows: [5, 6, 7, 8, 9, 10].map(makeFixtureWindow),
    counters: { slots: 64, finalizedCblocks: 16, skips: 0, averageCommitSpreadUs: 5800 }, parameters: fixtureParameters }
}

/** Named entry points for the frontend's at-rest state controls. */
export const FIXTURE_IDS = {
  slot: 64, skippedSlot: 70, crashedWindowSlot: 80, externalSlot: 96, externalTransaction: fixtureHash('tx-96-0'),
  transaction: fixtureHash('tx-64-0'), sender: fixtureAddress('sender'),
  hub: 'FRA', pendingHub: 'LHR', inactiveHub: 'SIN', outsideFormedHub: 'SIN', outsideFormedWindow: outsideFormedWindow.number, run: 'run-last-look',
} as const
const exampleCertificate = slotsByState.finalized.cblock.certificates[0]!
export const certificateFixtures: Record<Certificate['kind'], Certificate> = {
  notarization: exampleCertificate,
  notar_fallback: { ...exampleCertificate, kind: 'notar_fallback', atUs: 850000 },
  fast_finalization: slotsByState.finalized.cblock.certificates[1]!,
  slow_finalization: { ...exampleCertificate, kind: 'slow_finalization', atUs: 900000, linkedConsensusSlot: 17 },
  skip: slotsByState.skipped.cblock.certificates[0]!,
}
export const hubFixtures = { normal: hubs, empty: [] as Hub[], pending_only: [london], active: [frankfurt, newYork, tokyo], inactive: [singapore] }
export const balanceFixtures = { normal: makeFixtureBalance(), unfunded: makeFixtureBalance(fixtureAddress('sender'), false) }
export const faucetFixtures = { normal: faucetGrant(), status: makeFaucetStatus(), limit_reached: makeFaucetStatus(fixtureAddress('sender'), true) }
export const slotCertificateFixtures = {
  notarized: { ...slotsByState.executed, cblock: { ...slotsByState.finalized.cblock, status: 'notarized' as const, certificates: [certificateFixtures.notarization], finalizedUs: null } },
  slow_finalization: { ...slotsByState.finalized, cblock: { ...slotsByState.finalized.cblock, certificates: [certificateFixtures.notarization, certificateFixtures.slow_finalization], finalizedUs: 900000 } },
}
export const plainTransactionsByState = Object.fromEntries(Object.entries(transactionsByState).map(([state, original]) => {
  const tx = structuredClone(original)
  tx.kind = 'plain'; tx.t = 1; tx.n = 3
  if (state === 'partly_included') return [state, undefined]
  return [state, tx]
}).filter(([, tx]) => tx !== undefined)) as Partial<Record<Transaction['state'], Transaction>>
export const membershipFixtures = {
  joined: hubs[0]!.members[0]!,
  moving: { ...hubs[0]!.members[0]!, pendingMove: 'TYO', effectiveEpoch: 43, cooldownUntilEpoch: 45 },
  leaving: { ...hubs[0]!.members[0]!, pendingLeave: true, effectiveEpoch: 43, cooldownUntilEpoch: 45 },
}
export { replaySlot, replayRange } from './replay-fixture.js'

// The bridge as the local round trip left it: 2,500 deposited and credited, 1,000 withdrawn and
// released, a second withdrawal certified and waiting for its release.
const bridgeProgram = 'EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5'
const bridgeSolanaMint = '3A5GLMkvQndVrzcTTC7gXDUnBnaTDhQFWvumfhQ7uYyT'
const bridgeAcruxMint = 'Bu5CMp2GSnpzLGNpVvpA3mMPPftbiJRYBTpqYGSAJxsN'
export const bridgeFixture: z.infer<typeof Bridge> = {
  status: 'live', program: bridgeProgram, fromSlot: 0, relayer: null, native: null,
  lightClient: { epoch: 1, validators: 12, latestCertified: { consensusSlot: 174, epoch: 0, hash: 'a7'.repeat(32), outboxRoot: '5c'.repeat(32), account: '7Wt8f36u8o2DdMmZaQQpdpSg8QFS6hEdTheKZqtZEgvb' } },
  vaults: [{ solanaMint: bridgeSolanaMint, acruxMint: bridgeAcruxMint, decimals: 6, lockedBaseUnits: '1500000000', bridgedBaseUnits: '1250000000' }],
  totals: { deposits: 1, credited: 1, withdrawals: 2, released: 1 },
  readAt: '2026-11-18T14:30:00.000Z',
}
export const bridgeWaitingFixture: z.infer<typeof Bridge> = {
  status: 'waiting', program: null, fromSlot: null, relayer: null, native: null, lightClient: { epoch: null, validators: 0, latestCertified: null }, vaults: [],
  totals: { deposits: 0, credited: 0, withdrawals: 0, released: 0 }, readAt: null,
}
export const bridgeTransfersFixture: z.infer<typeof BridgeTransfer>[] = [
  { kind: 'withdrawal', sequence: 1, status: 'certified', solanaMint: bridgeSolanaMint, acruxMint: bridgeAcruxMint, amount: '250000000', from: 'BanDYHQEj35pryGMkXM8Ka1WobTdZavgPG97fcgi9pay', to: 'B7N7vHRUyM6dFb34fFtYqEucL1cPu6gi4UWDfRfYGSST', solanaSlot: null, acruxSlot: 698, cblock: 174, releaseAccount: null, releaseError: null },
  { kind: 'withdrawal', sequence: 0, status: 'released', solanaMint: bridgeSolanaMint, acruxMint: bridgeAcruxMint, amount: '1000000000', from: 'BanDYHQEj35pryGMkXM8Ka1WobTdZavgPG97fcgi9pay', to: 'B7N7vHRUyM6dFb34fFtYqEucL1cPu6gi4UWDfRfYGSST', solanaSlot: null, acruxSlot: 672, cblock: 168, releaseAccount: '9pz8cL3ALbkqF4QHBRewTWMSWX3PuGs4uPnNNSZvMETD', releaseError: null },
  { kind: 'deposit', sequence: 0, status: 'credited', solanaMint: bridgeSolanaMint, acruxMint: bridgeAcruxMint, amount: '2500000000', from: 'G9Hx9dH8tNCKdL1FogM2Qsy493Hp6SrG6re1hxkq5aD6', to: 'BanDYHQEj35pryGMkXM8Ka1WobTdZavgPG97fcgi9pay', solanaSlot: 43, acruxSlot: null, cblock: null, releaseAccount: null, releaseError: null },
]

// Token-page states use six-hour bridge epochs and finalized Solana supply observations.
const burnSupply = { amount: '999999000000000', decimals: 6, sampledAt: '2026-11-18T12:00:05.000Z' }
const burnedEpoch: BurnEpoch = {
  epoch: 42, firstSlot: 18144000, lastSlot: 18575999,
  startedAt: '2026-11-18T00:00:00.000Z', endedAt: '2026-11-18T06:00:00.000Z', closed: true,
  baseFeeBurnedLamports: '1000500', feeBurnAmountBaseUnits: '1000', remainderLamports: '500',
  solanaBurn: { signature: fixtureSignature('epoch-42-burn'), state: 'burned' },
  buybackBurnBaseUnits: '250000000', mintSupplyAtEnd: { ...burnSupply, amount: '999999500000000', sampledAt: '2026-11-18T06:00:04.000Z' },
}
const pendingEpoch: BurnEpoch = {
  ...burnedEpoch, epoch: 43, firstSlot: 18576000, lastSlot: 19007999,
  startedAt: '2026-11-18T06:00:00.000Z', endedAt: '2026-11-18T12:00:00.000Z',
  baseFeeBurnedLamports: '2000250', feeBurnAmountBaseUnits: '2000', remainderLamports: '750',
  solanaBurn: { signature: fixtureSignature('epoch-43-pending'), state: 'pending' },
  buybackBurnBaseUnits: '500000000', mintSupplyAtEnd: burnSupply,
}
const runningEpoch: BurnEpoch = {
  ...pendingEpoch, epoch: 44, firstSlot: 19008000, lastSlot: 19439999,
  startedAt: '2026-11-18T12:00:00.000Z', endedAt: null, closed: false,
  baseFeeBurnedLamports: '50000', feeBurnAmountBaseUnits: null, remainderLamports: null,
  solanaBurn: { signature: null, state: 'not_yet_due' }, buybackBurnBaseUnits: '0', mintSupplyAtEnd: null,
}
export const burnNoEpochs: Burn = {
  mint: bridgeSolanaMint, mintSupply: burnSupply, totals: { baseFeeBurnBaseUnits: '0', buybackBurnBaseUnits: '0' },
  epochs: { items: [], page: { nextCursor: null, hasMore: false }, asOf: burnSupply.sampledAt },
}
export const burnBurned: Burn = { ...burnNoEpochs, totals: { baseFeeBurnBaseUnits: '1000', buybackBurnBaseUnits: '250000000' }, epochs: { ...burnNoEpochs.epochs, items: [burnedEpoch] } }
export const burnPending: Burn = { ...burnBurned, totals: { baseFeeBurnBaseUnits: '1000', buybackBurnBaseUnits: '750000000' }, epochs: { ...burnNoEpochs.epochs, items: [pendingEpoch, burnedEpoch] } }
export const burnRunning: Burn = { ...burnPending, epochs: { ...burnNoEpochs.epochs, items: [runningEpoch, pendingEpoch, burnedEpoch] } }

/** Genesis stake 12,000,000 ACRUX; outside operators stake from epoch 13 and form a hub at epoch 14. */
export const genesisShare: GenesisShare = { currentEpoch: 14, epochs: [
  { epoch: 12, fixedAtSlot: null, genesisStake: '12000000000000000', totalStake: '12000000000000000', genesisShareBps: 10000, genesisMembers: 12, externalMembers: 0 },
  { epoch: 13, fixedAtSlot: 5615600, genesisStake: '12000000000000000', totalStake: '12120000000000000', genesisShareBps: 9900, genesisMembers: 12, externalMembers: 1 },
  { epoch: 14, fixedAtSlot: 6047600, genesisStake: '12000000000000000', totalStake: '12600000000000000', genesisShareBps: 9523, genesisMembers: 12, externalMembers: 5 },
  { epoch: 15, fixedAtSlot: 6479600, genesisStake: '12000000000000000', totalStake: '13200000000000000', genesisShareBps: 9090, genesisMembers: 12, externalMembers: 8 },
] }
export const genesisShareGenesisOnly: GenesisShare = { currentEpoch: 12, epochs: [genesisShare.epochs[0]!] }

/** Existing fees move to validators; these are synthetic observations, with no issuance. */
export const hubEarnings: HubEarnings = { hubCode: 'FRA', epochSlots: 432000, epochs: [
  { epoch: 43, countedFromSlot: 43 * 432000, rows: [
    { identity: frankfurt.members[0]!.identity, validatorKind: 'genesis', base: '105000', priority: '9100', inclusion: '42000' },
    { identity: outsideFixtureIdentity, validatorKind: 'external', base: '95000', priority: '15000', inclusion: '36000' },
  ] },
  { epoch: 42, countedFromSlot: 42 * 432000 + 128, rows: [
    { identity: frankfurt.members[0]!.identity, validatorKind: 'genesis', base: '25000', priority: '5000', inclusion: '12000' },
    { identity: outsideFixtureIdentity, validatorKind: 'external', base: '0', priority: '0', inclusion: '0' },
  ] },
] }
export const outsideHubEarnings: HubEarnings = { hubCode: 'SIN', epochSlots: 432000, epochs: [
  { epoch: 43, countedFromSlot: 43 * 432000, rows: outsideFormedHub.members.map((member, index) => ({
    identity: member.identity, validatorKind: 'external', base: index === 3 ? '0' : '75000', priority: index === 3 ? '0' : '10000', inclusion: index === 3 ? '0' : '30000',
  })) },
] }
export const hubEarningsEmpty: HubEarnings = { hubCode: 'FRA', epochSlots: 432000, epochs: [] }
