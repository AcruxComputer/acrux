import assert from 'node:assert/strict'
import { z } from 'zod'
import * as s from '../dist/schemas.js'
import * as f from '../dist/fixtures.js'
import { page, PageInfo } from '../dist/primitives.js'
import { ProgramError, baseErrors } from '../dist/errors.js'
import { ProtocolEvent, LabReplay, SubscribeFilter, StreamCommand } from '../dist/events.js'
import { rpcMethods, RpcRequest, RpcFailure } from '../dist/rpc.js'

// Run after the contract build. Exercise the same parsers shipped to API clients.
const extra = '__futureResponseField'
const address = f.fixtureAddress('compatibility')
const signature = f.fixtureSignature('compatibility')
const slot = f.slotsByState.finalized
const transaction = f.makeFixtureTransaction()
const snapshot = { ...f.makeNetSnapshot(), recentSlots: [slot] }
const replay = f.makeLabReplay(f.labRuns[1])
const error = { ...f.failureFixtures.CHAIN_REFUSED, error: { ...f.failureFixtures.CHAIN_REFUSED.error,
  programError: { programId: address, name: 'ExampleProgramError', number: 1 } } }
const paged = items => ({ items, page: { nextCursor: null, hasMore: false }, asOf: f.FIXTURE_TIME })
const quote = { inputMint: f.demoPool.mints[0].address, outputMint: f.demoPool.mints[1].address,
  amountIn: '100', amountOut: '199', minimumOut: '190', feeAmount: '1', slot: 64 }
const relayer = { wallet: address, lamports: '1000000', unpaid: 1, lastCblock: 16, lastHandedEpoch: 42, lastRelayedAt: f.FIXTURE_TIME }
const native = { cutoverSlot: 64, mint: address, scale: '1000', reserveLamports: '1000000', creditedLamports: '100000',
  withdrawnLamports: '1000', circulatingLamports: '98000', feeBurnedLamports: '1000' }
const buybackRow = { id: 'burn-1', cycleId: 'cycle-1', action: 'burn', signature, lamports: '0', tokenAmount: '100', createdAt: f.FIXTURE_TIME }
const responseFixtures = {
  Parameters: f.fixtureParameters, Proposer: slot.window.proposers[0], Window: slot.window,
  Slice: slot.blocks[0].slices[0], CommitmentArrival: slot.blocks[0].commitment.arrivals[0],
  Commitment: slot.blocks[0].commitment, Attestation: slot.attestations[0], ProposerBlock: slot.blocks[0],
  TxHeader: transaction.header, FragmentInclusion: transaction.fragments[0], MergedTransaction: slot.mergedOrder[0],
  Placement: slot.schedule.placements[0], ExecutionSchedule: slot.schedule, Skip: f.slotsByState.skipped.skip,
  Vote: slot.cblock.votes[0], Certificate: slot.cblock.certificates[0], Cblock: slot.cblock, Slot: slot,
  TxFees: { base: '5000', priority: '100', inclusion: '3000', burned: '2500', total: '8100' },
  Transaction: transaction, Counters: snapshot.counters, NetSnapshot: snapshot,
  Balance: f.makeFixtureBalance(), DemoMint: f.demoPool.mints[0], DemoPool: f.demoPool, Quote: quote,
  Submission: { id: transaction.header.id, accepted: true, targetSlot: 64 },
  Member: f.outsideFormedHub.members[0], HashExchangeStats: f.facts.commitSpreadByLinkUs.applied,
  HashExchangePair: f.hashExchangeWithOutside.hubs[0].pairs[0], HashExchangeSpread: f.facts.commitSpreadByLinkUs,
  HashExchangeHub: f.hashExchangeWithOutside.hubs[0], HashExchange: f.hashExchangeWithOutside,
  HashExchangeTelemetry: { ...f.hashExchangeWithOutside, commitSpreadByLinkUs: f.factsWithOutside.commitSpreadByLinkUs },
  HubHashExchange: f.outsideFormedHub.hashExchange, Hub: f.outsideFormedHub, Hubs: paged(f.hubs), GenesisShare: f.genesisShare,
  HubEarningsRow: f.hubEarnings.epochs[0].rows[1], HubEarningsEpoch: f.hubEarnings.epochs[0], HubEarnings: f.hubEarnings,
  RegistryConfig: f.registryConfig, SchedulePreview: f.schedulePreview, FaucetPolicy: f.faucetPolicy,
  FaucetStatus: f.makeFaucetStatus(), FaucetGrant: f.faucetGrant(), Facts: f.factsWithOutside,
  Evidence: f.labRuns[0].evidence[0], LabScenario: f.scenarios[0], LabRun: f.labRuns[0],
  LabRuns: paged(f.labRuns), SpecMap: f.specMap,
  BridgeVault: f.bridgeFixture.vaults[0], BridgeCertified: f.bridgeFixture.lightClient.latestCertified,
  BridgeRelayer: relayer, BridgeNative: native, Bridge: { ...f.bridgeFixture, relayer, native },
  BridgeTransfer: f.bridgeTransfersFixture[0], BridgeTransfers: paged(f.bridgeTransfersFixture),
  BuybackRow: buybackRow, Buyback: { status: 'live', mint: address, wallet: address,
    totals: { claimedLamports: '100', swappedLamports: '100', acruxBought: '100', acruxBurned: '100' }, rows: [buybackRow] },
  ReplayLeaderItem: f.replaySlot.leaderOrder[0], ReplayMergedItem: f.replaySlot.mergedOrder[0],
  ReplayRepricing: f.replaySlot.sandwiches.find(item => item.repricing !== null).repricing,
  ReplaySandwich: f.replaySlot.sandwiches[0], ReplaySlotRecord: f.replaySlot,
  ReplaySandwichRow: f.replayRange.sandwiches[0], ReplayRange: f.replayRange,
  BurnSupply: f.burnBurned.mintSupply, BurnEpoch: f.burnBurned.epochs.items[0], Burn: f.burnRunning,
  Health: { status: 'ready', contractVersion: s.CONTRACT_VERSION, missingConfiguration: [] }, ApiError: error,
}
const requestNames = new Set(['TxHeaderRequest', 'QuoteRequest', 'PlainSubmission', 'FragmentSubmission', 'FaucetRequest',
  'LabRunRequest', 'PageQuery', 'BridgeTransfersQuery'])
const objectResponses = Object.entries(s).filter(([name, schema]) => schema instanceof z.ZodObject && !requestNames.has(name))
assert.deepEqual(Object.keys(responseFixtures).sort(), objectResponses.map(([name]) => name).sort(),
  'Every exported response object needs an explicit fixture in this check')

let objectSites = 0
function withAdditions(schema, value) {
  if (value === null || value === undefined) return value
  if (schema instanceof z.ZodObject) {
    objectSites++
    return { ...Object.fromEntries(Object.entries(value).map(([key, item]) =>
      [key, schema.shape[key] ? withAdditions(schema.shape[key], item) : item])), [extra]: 'additive' }
  }
  if (schema instanceof z.ZodArray) return value.map(item => withAdditions(schema.element, item))
  if (schema instanceof z.ZodTuple) return value.map((item, index) => withAdditions(schema.def.items[index], item))
  if (schema instanceof z.ZodRecord) return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, withAdditions(schema.valueType, item)]))
  if (schema instanceof z.ZodUnion) {
    const variant = schema.options.find(option => option.safeParse(value).success)
    assert(variant, 'Fixture must match a union member')
    return withAdditions(variant, value)
  }
  if (schema instanceof z.ZodLazy) return withAdditions(schema.def.getter(), value)
  if (schema.def.innerType) return withAdditions(schema.def.innerType, value)
  return value
}
function acceptsAdditions(name, schema, fixture) {
  const expected = schema.parse(fixture)
  assert.deepEqual(schema.parse(withAdditions(schema, fixture)), expected, `${name} strips additive fields at every response object`)
}
for (const [name, schema] of objectResponses) acceptsAdditions(name, schema, responseFixtures[name])
for (const [name, schema, fixture] of [
  ['PageInfo', PageInfo, paged([]).page], ['page', page(s.Hub), paged([f.outsideFormedHub])],
  ['ProgramError', ProgramError, error.error.programError], ['baseErrors.ApiError', baseErrors.ApiError, error],
  ['LabReplay', LabReplay, replay], ['LabScenarios', z.array(s.LabScenario), f.scenarios],
  ['RpcFailure', RpcFailure, { jsonrpc: '2.0', id: 1, error: { code: -32000, message: 'Refused', data: error } }],
]) acceptsAdditions(name, schema, fixture)

const control = { version: 1, sequence: 0, streamId: 'compatibility', slot: null, anchor: 'stream', atUs: 0 }
const events = new Map(Object.values(f.labReplays).flatMap(item => item.events).map(event => [event.type, event]))
for (const [type, data] of [
  ['stream.snapshot', snapshot], ['stream.resync', { reason: 'restarted', snapshot }],
  ['stream.heartbeat', { latestSlot: 64, status: 'live' }], ['stream.error', error], ['net.counters', snapshot.counters],
]) events.set(type, { ...control, type, data })
assert.equal(events.size, ProtocolEvent.options.length, 'Every response event variant has a fixture')
for (const event of events.values()) acceptsAdditions(`ProtocolEvent:${event.type}`, ProtocolEvent, event)

const feeBurns = { program: address, mint: address, nativeFromSlot: 64, epochSlots: 432000,
  items: [{ epoch: 42, firstSlot: 18144000, lastSlot: 18575999, startedAt: 0, endedAt: 21600000,
    closed: true, baseFeeBurnedLamports: '1000500', feeBurnAmountBaseUnits: '1000', remainderLamports: '500' }],
  nextCursor: null, feeBurnedBaseUnits: '1000' }
const rpcFixtures = {
  getValidatorEarningsEpochs: { epochSlots: f.hubEarnings.epochSlots, epochs: f.hubEarnings.epochs.map(epoch => epoch.epoch) },
  getValidatorEarnings: { ...f.hubEarnings.epochs[0], rows: f.hubEarnings.epochs[0].rows.map(row => ({ ...row, hub: 'FRA' })) },
  getFeeBurns: feeBurns, getSnapshot: snapshot, getSlot: slot, getRecentSlots: [slot], getCurrentWindow: slot.window,
  getSchedule: [slot.window], getCounters: snapshot.counters, getTransaction: transaction,
  getLatestBlockhash: { blockhash: address, slot: 64, startedSlot: 64 }, getFacts: f.factsWithOutside,
  getBalance: f.makeFixtureBalance(), getDemoPool: f.demoPool, getQuote: quote,
  submitTransaction: responseFixtures.Submission, submitFragment: responseFixtures.Submission, getLabReplay: replay,
}
assert.deepEqual(Object.keys(rpcFixtures).sort(), Object.keys(rpcMethods).sort(), 'Every RPC result has a fixture')
for (const [name, method] of Object.entries(rpcMethods)) acceptsAdditions(`rpc:${name}`, method.result, rpcFixtures[name])

// A reader with the older shape accepts new hub metadata and strips it.
const olderHub = s.Hub.omit({ formedOutside: true, activatedAtEpoch: true, hashExchange: true }).extend({
  members: z.array(s.Member.omit({ validatorKind: true, admittedAtEpoch: true })),
})
const oldView = olderHub.parse(withAdditions(s.Hub, f.outsideFormedHub))
assert.equal(Object.hasOwn(oldView, 'formedOutside'), false)
assert.equal(Object.hasOwn(oldView, 'activatedAtEpoch'), false)
assert.equal(Object.hasOwn(oldView.members[0], 'validatorKind'), false)
acceptsAdditions('older-shaped Hub fixture', s.Hub, oldView)
const oldProposer = s.Proposer.omit({ validatorKind: true }).parse(f.externalBlockSlot.window.proposers[0])
acceptsAdditions('older-shaped Proposer fixture', s.Proposer, oldProposer)
const olderFacts = s.Facts.omit({ hashExchange: true, commitSpreadByLinkUs: true })
const oldFacts = olderFacts.parse(withAdditions(s.Facts, f.factsWithOutside))
assert.equal(Object.hasOwn(oldFacts, 'hashExchange'), false)
assert.equal(Object.hasOwn(oldFacts, 'commitSpreadByLinkUs'), false)
acceptsAdditions('older-shaped Facts fixture', s.Facts, oldFacts)
assert.equal(s.Hub.safeParse({ ...f.outsideFormedHub, formedOutside: 'yes' }).success, false, 'Known field types still validate')
assert.equal(s.Slot.safeParse({ ...slot, slot: -1 }).success, false, 'Known field bounds still validate')

const quoteRequest = { inputMint: address, amountIn: '1', slippageBps: 100 }
const plain = { header: transaction.header, transaction: 'AA==', receivers: [1] }
const fragment = { header: transaction.header, n: 3, t: 2, index: 0, receiver: 1,
  fragment: 'AA==', keyShare: 'AA==', headerSignature: signature }
const requests = [
  ['TxHeaderRequest', s.TxHeaderRequest, transaction.header], ['QuoteRequest', s.QuoteRequest, quoteRequest],
  ['PlainSubmission', s.PlainSubmission, plain], ['FragmentSubmission', s.FragmentSubmission, fragment],
  ['FaucetRequest', s.FaucetRequest, { address }], ['LabRunRequest', s.LabRunRequest, { scenario: 'crash' }],
  ['PageQuery', s.PageQuery, {}], ['BridgeTransfersQuery', s.BridgeTransfersQuery, { address, kind: 'withdrawal' }],
  ['RpcRequest', RpcRequest, { jsonrpc: '2.0', id: 1, method: 'getSlot', params: { slot: 64 } }],
  ...[{ kind: 'all' }, { kind: 'slots', from: 64, to: 67 }, { kind: 'transaction', id: transaction.header.id }]
    .map(value => [`SubscribeFilter:${value.kind}`, SubscribeFilter, value]),
  ['StreamCommand:subscribe', StreamCommand, { version: 1, type: 'subscribe', filter: { kind: 'all' } }],
  ['StreamCommand:unsubscribe', StreamCommand, { version: 1, type: 'unsubscribe' }],
]
const rpcParams = {
  getValidatorEarningsEpochs: {}, getValidatorEarnings: { epoch: 43 },
  getFeeBurns: {}, getSnapshot: {}, getSlot: { slot: 64 }, getRecentSlots: {}, getCurrentWindow: {},
  getSchedule: {}, getCounters: {}, getTransaction: { id: transaction.header.id }, getLatestBlockhash: {}, getFacts: {},
  getBalance: { address }, getDemoPool: {}, getQuote: quoteRequest, submitTransaction: plain,
  submitFragment: fragment, getLabReplay: { runId: replay.run.id },
}
for (const [name, method] of Object.entries(rpcMethods)) requests.push([`rpc.params:${name}`, method.params, rpcParams[name]])
requests.push(['rpc.params:getTransaction.signature', rpcMethods.getTransaction.params, { signature }])
for (const [name, schema, fixture] of requests) {
  schema.parse(fixture)
  assert.equal(schema.safeParse({ ...fixture, [extra]: 'rejected' }).success, false, `${name} rejects unknown request keys`)
}
for (const [name, schema, fixture] of [['PlainSubmission', s.PlainSubmission, plain], ['FragmentSubmission', s.FragmentSubmission, fragment]]) {
  assert.equal(schema.safeParse({ ...fixture, header: { ...fixture.header, [extra]: 'rejected' } }).success, false,
    `${name} rejects unknown nested header fields`)
}
assert.equal(StreamCommand.safeParse({ version: 1, type: 'subscribe', filter: { kind: 'all', [extra]: 'rejected' } }).success, false,
  'StreamCommand rejects unknown nested filter fields')
console.log(JSON.stringify({ check: 'response-compatibility', responseObjects: objectResponses.length, auxiliaryResponses: 7,
  eventVariants: events.size, rpcResults: Object.keys(rpcMethods).length, nestedResponseObjects: objectSites,
  strictRequestCases: requests.length, strictNestedRequests: 3, olderShapes: 3, result: 'passed' }))
