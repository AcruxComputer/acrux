import { z } from 'zod'
import { Address, Base64, BaseUnits, Signature, Timestamp, U64, page, PageQuery } from './primitives.js'
import { defineErrors } from './errors.js'
export { Address, Base64, BaseUnits, Signature, Timestamp, U64, PageQuery }
export { CONTRACT_VERSION } from './version.js'
export const errors = defineErrors({ FAUCET_LIMIT: 429, FAUCET_EMPTY: 503, FAUCET_RETIRED: 410, GRANT_FAILED: 502, NET_STALLED: 503, NET_UNAVAILABLE: 503, INVALID_INPUT: 400 })
export const ApiError = errors.ApiError
export const ErrorCode = errors.ErrorCode
export type ErrorCode = z.infer<typeof ErrorCode>
export const Count = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER)
export const SlotNumber = Count
export const Hash = z.string().regex(/^[a-f0-9]{64}$/)
export const TxId = Hash
// Registry format is configured at runtime; do not freeze undecided code rules here.
export const HubCode = z.string().min(1)
export const ProposerIndex = z.number().int().min(1).max(3)
export const Track = z.number().int().min(1).max(4)
export const Parameters = z.strictObject({
  k: z.literal(3), kappa: z.literal(4), w: z.literal(16), deltaLocalUs: z.literal(10000),
  deltaBlockUs: z.literal(50000), deltaGlobalUs: z.literal(250000), timeoutUs: z.literal(750000),
  tracks: z.literal(4), reserveLamports: z.literal('1000000'), trackCapacityCu: Count.nullable(),
  fragmentExpirySlots: Count.nullable(), alpha: z.number().min(0).max(1).nullable(),
  minimumHubMembers: Count.nullable(), minimumHubStake: U64.nullable(),
})
export type Parameters = z.infer<typeof Parameters>
export const PROTOCOL_PARAMETERS: Parameters = {
  k: 3, kappa: 4, w: 16, deltaLocalUs: 10000, deltaBlockUs: 50000, deltaGlobalUs: 250000,
  timeoutUs: 750000, tracks: 4, reserveLamports: '1000000', trackCapacityCu: null,
  fragmentExpirySlots: null, alpha: null, minimumHubMembers: null, minimumHubStake: null,
}
export const Proposer = z.strictObject({ index: ProposerIndex, identity: Address, stake: U64, status: z.enum(['online', 'late', 'crashed']) })
export type Proposer = z.infer<typeof Proposer>
export const Window = z.strictObject({ number: Count, hubCode: HubCode, firstSlot: SlotNumber, lastSlot: SlotNumber, consensusSlots: z.array(Count), proposers: z.array(Proposer).length(3) })
export type Window = z.infer<typeof Window>
export const Slice = z.strictObject({ index: Count, shredCount: z.literal(64), rebuildShreds: z.literal(32), bytes: Count, sentUs: Count, arrivedUs: Count, merkleRoot: Hash })
export const CommitmentArrival = z.strictObject({ receiver: ProposerIndex, startUs: Count, arrivedUs: Count, relativeToStartUs: z.number().int(), deadlineUs: z.literal(20000), onTime: z.boolean() })
export const Commitment = z.strictObject({ slot: SlotNumber, proposer: ProposerIndex, hash: Hash, keyHash: Hash, signature: Signature, sentUs: Count, arrivals: z.array(CommitmentArrival) })
export type Commitment = z.infer<typeof Commitment>
export const Attestation = z.strictObject({ slot: SlotNumber, proposer: ProposerIndex, pi: z.array(Hash.nullable()).length(3), key: Base64, signature: Signature, sentUs: Count, validatorArrivals: z.array(z.strictObject({ identity: Address, arrivedUs: Count })) })
export type Attestation = z.infer<typeof Attestation>
export const ProposerBlock = z.strictObject({ proposer: ProposerIndex, identity: Address, status: z.enum(['streaming', 'sealed', 'accepted', 'dropped', 'missing']), bytes: Count, slices: z.array(Slice), commitment: Commitment.nullable(), attestation: Attestation.nullable(), parent: z.strictObject({ consensusSlot: Count, hash: Hash }).nullable() })
export type ProposerBlock = z.infer<typeof ProposerBlock>
/** priorityFeePerCu is an unsigned decimal string of micro-lamports per compute unit. */
export const TxHeader = z.strictObject({ id: TxId, feePayer: Address, priorityFeePerCu: U64.describe('Micro-lamports per compute unit, encoded as an unsigned decimal string.'), computeLimit: Count.min(1), writableAccounts: z.array(Address), targetSlot: SlotNumber })
export type TxHeader = z.infer<typeof TxHeader>
export const FragmentInclusion = z.strictObject({ index: Count, receiver: ProposerIndex, slot: SlotNumber, proposer: ProposerIndex, includedUs: Count })
export const MergedTransaction = TxHeader.extend({ kind: z.enum(['plain', 'fragments']), sourceProposers: z.array(ProposerIndex), n: z.number().int().min(1).max(3), t: z.number().int().min(1).max(3), fragmentIndices: z.array(Count), position: Count, selected: z.boolean(), exclusion: z.enum(['capacity', 'priority_fee', 'incomplete']).nullable() })
export type MergedTransaction = z.infer<typeof MergedTransaction>
export const Placement = z.strictObject({ track: Track, startCu: Count, lengthCu: Count, txId: TxId })
export type Placement = z.infer<typeof Placement>
export const ExecutionSchedule = z.strictObject({ algorithm: z.literal(4), tracks: z.literal(4), capacityCu: Count.min(1), placements: z.array(Placement), gaps: z.array(z.strictObject({ track: Track, startCu: Count, lengthCu: Count })) })
export type ExecutionSchedule = z.infer<typeof ExecutionSchedule>
export const SkipReason = z.enum(['disagreeing_vectors', 'missing_attestation', 'equivocation', 'majority_lost', 'window_skipped'])
export const Skip = z.strictObject({ reason: SkipReason, detail: z.string(), disagreeingVectors: z.array(Attestation), affectedSlots: z.array(SlotNumber), atUs: Count })
export const Vote = z.strictObject({ consensusSlot: Count, validator: Address, kind: z.enum(['notar', 'notar_fallback', 'skip', 'skip_fallback', 'finalize']), stakeShare: z.number().min(0).max(1), hash: Hash.nullable(), atUs: Count })
export type Vote = z.infer<typeof Vote>
export const Certificate = z.strictObject({ consensusSlot: Count, kind: z.enum(['notarization', 'notar_fallback', 'fast_finalization', 'slow_finalization', 'skip']), stakeShare: z.number().min(0).max(1), threshold: z.number().min(0).max(1), hash: Hash.nullable(), signers: z.array(Address), atUs: Count, linkedConsensusSlot: Count.nullable() })
export type Certificate = z.infer<typeof Certificate>
export const Cblock = z.strictObject({ consensusSlot: Count, slots: z.array(SlotNumber), parent: z.strictObject({ consensusSlot: Count, hash: Hash }).nullable(), hash: Hash.nullable(), stateHash: Hash.nullable(), votes: z.array(Vote), certificates: z.array(Certificate), status: z.enum(['pending', 'notarized', 'finalized', 'skipped']), finalizedUs: Count.nullable() })
export type Cblock = z.infer<typeof Cblock>
// Block and execution times are slot-relative; these two deadlines are window ParentReady-relative.
export const Slot = z.strictObject({ slot: SlotNumber, wallTime: Timestamp, window: Window, consensusSlot: Count, status: z.enum(['pending', 'executed', 'finalized', 'skipped']), blocks: z.array(ProposerBlock), attestations: z.array(Attestation), agreedSet: z.array(ProposerIndex).nullable(), skip: Skip.nullable(), mergedOrder: z.array(MergedTransaction), schedule: ExecutionSchedule.nullable(), stateHash: Hash.nullable(), xblockHash: Hash.nullable(), cblock: Cblock, executedUs: Count.nullable(), attestationDeadlineUs: Count, consensusTimeoutUs: Count })
export type Slot = z.infer<typeof Slot>
/** What the payer paid, in lamports: native ACRUX from the cut-over. Half the base fee is burned. */
export const TxFees = z.strictObject({ base: U64, priority: U64, inclusion: U64, burned: U64, total: U64 })
export type TxFees = z.infer<typeof TxFees>
export const Transaction = z.strictObject({ header: TxHeader, state: z.enum(['unknown', 'sent', 'partly_included', 'executed', 'finalized', 'failed', 'expired']), kind: z.enum(['plain', 'fragments']), n: z.number().int().min(1).max(3), t: z.number().int().min(1).max(3), receivers: z.array(Proposer), fragments: z.array(FragmentInclusion), includedCount: Count, readableSlot: SlotNumber.nullable(), executedSlot: SlotNumber.nullable(), mergedPosition: Count.nullable(), result: z.enum(['success', 'error']).nullable(), logs: z.array(z.string()), failure: z.string().nullable(), expiresAfterSlot: SlotNumber.nullable(), finalization: Certificate.nullable(), fees: TxFees.optional() })
export type Transaction = z.infer<typeof Transaction>
export const Counters = z.strictObject({ slots: Count, finalizedCblocks: Count, skips: Count, averageCommitSpreadUs: z.number().nonnegative() })
export const NetSnapshot = z.strictObject({ streamId: z.string(), cursor: Count, status: z.enum(['live', 'stalled', 'unavailable']), currentWindow: Window, recentSlots: z.array(Slot).max(64), upcomingWindows: z.array(Window), counters: Counters, parameters: Parameters })
export type NetSnapshot = z.infer<typeof NetSnapshot>
export const Balance = z.strictObject({ address: Address, lamports: U64, reserveLamports: U64, spendableLamports: U64, slot: SlotNumber })
export const DemoMint = z.strictObject({ address: Address, symbol: z.string(), name: z.string(), decimals: Count })
export const DemoPool = z.strictObject({ address: Address, mints: z.tuple([DemoMint, DemoMint]), reserves: z.tuple([U64, U64]), feeBps: Count, slot: SlotNumber })
export const QuoteRequest = z.strictObject({ inputMint: Address, amountIn: U64.refine(x => x !== '0'), slippageBps: Count.max(10000) })
export const Quote = z.strictObject({ inputMint: Address, outputMint: Address, amountIn: U64, amountOut: U64, minimumOut: U64, feeAmount: U64, slot: SlotNumber })
export const PlainSubmission = z.strictObject({ header: TxHeader, transaction: Base64.min(1), receivers: z.array(ProposerIndex).min(1).max(3) })
export const FragmentSubmission = z.strictObject({ header: TxHeader, n: z.number().int().min(2).max(3), t: z.number().int().min(2).max(3), index: Count.max(2), receiver: ProposerIndex, fragment: Base64.min(1), keyShare: Base64.min(1), headerSignature: Signature }).superRefine((v,c) => { if(v.t > v.n || v.index >= v.n) c.addIssue({code:'custom', message:'Require t <= n and fragment index < n.'}) })
export const Submission = z.strictObject({ id: TxId, accepted: z.literal(true), targetSlot: SlotNumber })
/** The chain a hub or membership lives on: Acrux's own Hub Registry, or the one on Solana mainnet. */
export const RegistryChain = z.enum(['acrux', 'solana'])
export const Member = z.strictObject({ chain: RegistryChain, identity: Address, hubCode: HubCode, stake: U64, proposerStake: U64, capped: z.boolean(), effectiveEpoch: Count, joinedEpoch: Count, pendingMove: HubCode.nullable(), pendingLeave: z.boolean(), cooldownUntilEpoch: Count,
  validatorKind: z.enum(['genesis', 'external']).optional(), admittedAtEpoch: Count.nullable().optional() })
export type Member = z.infer<typeof Member>
/** ECEF coordinates are integer metres in the WGS84 geocentric reference frame. */
export const Hub = z.strictObject({ chain: RegistryChain, code: HubCode, name: z.string(), region: z.string(), ecef: z.strictObject({ x: z.number().int(), y: z.number().int(), z: z.number().int() }).describe('Integer metres in WGS84 Earth-centred, Earth-fixed coordinates.'), latitude: z.number().min(-90).max(90), longitude: z.number().min(-180).max(180), registrant: Address, depositLamports: U64, members: z.array(Member), stake: U64, proposerStake: U64, proposerStakeCap: U64, status: z.enum(['active', 'pending', 'inactive']), needs: z.strictObject({ members: Count, stake: U64 }), createdEpoch: Count, lastActiveEpoch: Count.nullable(), releasableEpoch: Count.nullable(), releasable: z.boolean() })
export type Hub = z.infer<typeof Hub>
export const Hubs = page(Hub)
export const RegistryConfig = z.strictObject({ status: z.enum(['fixture', 'waiting', 'ready']), chain: RegistryChain, programId: Address.nullable(), cluster: z.enum(['localnet', 'mainnet-beta', 'acrux']), depositLamports: U64.nullable(), registrationFeeBps: Count.max(10000).nullable(), cooldownEpochs: Count.nullable(), inactivityEpochs: Count.nullable(), codeRules: z.strictObject({ pattern: z.string().nullable(), minLength: Count.nullable(), maxLength: Count.nullable() }), minimumMembers: Count.nullable(), minimumStake: U64.nullable(), currentEpoch: Count, missingConfiguration: z.array(z.string()) })
export const SchedulePreview = z.strictObject({ epoch: Count, seed: z.string(), source: z.enum(['fixture', 'registry']), algorithm: z.literal(3), alpha: z.number().min(0).max(1), totalWindows: Count, shares: z.array(z.strictObject({ hubCode: HubCode, proposerStake: U64, probability: z.number(), assignedWindows: Count })), regionalRuns: z.array(z.strictObject({ region: z.string(), firstWindow: Count, lastWindow: Count })), windows: z.array(Window) })
export const FaucetPolicy = z.strictObject({ amountLamports: U64.nullable(), perKey: Count.nullable(), perIp: Count.nullable(), periodSeconds: Count.nullable(), missingConfiguration: z.array(z.string()) })
/** `availableLamports`: what the faucet can still grant, in lamports; null while the net cannot be read. */
export const FaucetStatus = z.strictObject({ address: Address, policy: FaucetPolicy, remainingForKey: Count.nullable(), remainingForIp: Count.nullable(), resetsAt: Timestamp.nullable(), eligible: z.boolean(), availableLamports: U64.nullable() })
export const FaucetRequest = z.strictObject({ address: Address })
export const FaucetGrant = z.strictObject({ id: z.string(), transactionId: TxId.optional(), address: Address, amountLamports: U64, signature: Signature, balance: Balance, grantedAt: Timestamp })
export const Facts = z.strictObject({ source: z.enum(['fixture', 'measured']), measuredAt: Timestamp, sampleSize: Count, slotPacingUs: z.strictObject({ p50: Count, p99: Count }), commitSpreadUs: z.strictObject({ p50: Count, p99: Count }), attestationRelativeToBlockUs: z.strictObject({ p50: z.number().int(), p99: z.number().int() }), cblockFinalizationUs: z.strictObject({ p50: Count, p99: Count }),
  /** Window handovers per hub link (e.g. "FRA-NYC"): measured p50/p99 and the link's Rotor floor (50 ms plus the second hub's votes for the 60% certificate). */
  handoverUs: z.record(z.string(), z.strictObject({ p50: Count, p99: Count, floorUs: Count, count: Count })) })
export const ScenarioId = z.enum(['last-look', 'censorship', 'crash', 'equivocation', 'split-commitments', 'private-front-run'])
export const Evidence = z.strictObject({ kind: z.enum(['late_block_dropped', 'transaction_included', 'two_proposers_continue', 'slashing_proof', 'skip_certificate', 'unreadable_fragment']), slot: SlotNumber, proposer: ProposerIndex.nullable(), txId: TxId.nullable(), description: z.string(), hashes: z.array(Hash), signatures: z.array(Signature), observedFragments: Count.nullable(), threshold: Count.nullable() })
export const LabScenario = z.strictObject({ id: ScenarioId, title: z.string(), description: z.string(), seed: z.string(), parameters: z.record(z.string(), z.union([z.string(), z.number(), z.boolean()])), lemma: z.string(), expectedOutcome: z.string() })
export const LabRunRequest = z.strictObject({ scenario: ScenarioId, seed: z.string().min(1).max(128).optional() })
export const LabRun = z.strictObject({ id: z.string(), scenario: LabScenario, seed: z.string(), status: z.enum(['queued', 'running', 'done', 'failed_to_start']), createdAt: Timestamp, startedAt: Timestamp.nullable(), endedAt: Timestamp.nullable(), outcome: z.string().nullable(), evidence: z.array(Evidence), failure: z.string().nullable() })
export type LabRun = z.infer<typeof LabRun>
export const LabRuns = page(LabRun)
export const SpecMap = z.strictObject({ paper: z.string(), parameters: Parameters, entries: z.array(z.strictObject({ reference: z.string(), lines: z.string().nullable(), file: z.string(), function: z.string(), scenario: ScenarioId.nullable(), status: z.enum(['fixture', 'planned', 'implemented']) })), decisions: z.array(z.strictObject({ topic: z.string(), decision: z.string(), status: z.enum(['decided', 'configuration_required', 'fixture_only']) })) })
/**
 * The bridge between Solana and Acrux (`acrux_bridge`, the same program on both chains). A deposit
 * locks tokens in the Solana mint's vault and is credited on Acrux at the mint derived from it, with
 * the same decimals; a withdrawal burns on Acrux and is released from the vault once a certified
 * cblock proves it. Amounts are base units.
 */
export const BridgeVault = z.strictObject({
  solanaMint: Address, acruxMint: Address, decimals: z.number().int().min(0).max(255),
  /** What the vault on Solana holds. */
  lockedBaseUnits: U64,
  /** The bridged mint's supply on Acrux. */
  bridgedBaseUnits: U64,
})
/** The newest cblock Solana's light client certified. */
export const BridgeCertified = z.strictObject({ consensusSlot: Count, epoch: Count, hash: Hash, outboxRoot: Hash, account: Address })
export const BridgeRelayer = z.strictObject({
  wallet: Address, lamports: U64, unpaid: Count, lastCblock: Count.nullable(), lastHandedEpoch: Count.nullable(), lastRelayedAt: Timestamp.nullable(),
})
/** Native backing counters after the configured cut-over; all amounts are native lamports. */
export const BridgeNative = z.strictObject({
  cutoverSlot: Count, mint: Address, scale: U64,
  reserveLamports: U64, creditedLamports: U64, withdrawnLamports: U64, circulatingLamports: U64,
  /** Cumulative native lamports committed in rounded FeeBurn entries, including burns waiting on Solana. */
  feeBurnedLamports: U64,
})
export type BridgeNative = z.infer<typeof BridgeNative>
export const Bridge = z.strictObject({
  status: z.enum(['waiting', 'live']),
  /** The program's address, on Solana and on Acrux. */
  program: Address.nullable(),
  /** The first Acrux slot of the bridge rule. */
  fromSlot: Count.nullable(),
  lightClient: z.strictObject({ epoch: Count.nullable(), validators: Count, latestCertified: BridgeCertified.nullable() }),
  relayer: BridgeRelayer.nullable(),
  native: BridgeNative.nullable(),
  vaults: z.array(BridgeVault),
  totals: z.strictObject({ deposits: Count, credited: Count, withdrawals: Count, released: Count }),
  readAt: Timestamp.nullable(),
})
/**
 * A deposit is `locked` on Solana, then `credited` on Acrux. A withdrawal is `burned` on Acrux, then
 * `certified` once Solana's light client certified its cblock, then `released` from the vault.
 */
export const BridgeTransferStatus = z.enum(['locked', 'credited', 'burned', 'certified', 'released'])
export const BridgeTransfer = z.strictObject({
  kind: z.enum(['deposit', 'withdrawal']), sequence: Count, status: BridgeTransferStatus,
  solanaMint: Address, acruxMint: Address, amount: U64,
  /** The depositor on Solana, or the withdrawal's owner on Acrux. */
  from: Address,
  /** The Acrux recipient of a deposit, or the Solana recipient of a withdrawal. */
  to: Address,
  /** A deposit's Solana slot. */
  solanaSlot: Count.nullable(),
  /** A withdrawal's Acrux slot, and the cblock whose outbox carries it. */
  acruxSlot: Count.nullable(), cblock: Count.nullable(),
  /** The bitmap account on Solana containing this withdrawal's released bit. */
  releaseAccount: Address.nullable(),
  /** Last failed payout reason while the withdrawal remains unpaid. */
  releaseError: z.string().nullable(),
})
export const BridgeTransfers = page(BridgeTransfer)
export const BridgeTransfersQuery = PageQuery.extend({ address: Address.optional(), kind: z.enum(['deposit', 'withdrawal']).optional() })
export type BridgeVault = z.infer<typeof BridgeVault>
export type Bridge = z.infer<typeof Bridge>
export type BridgeTransferStatus = z.infer<typeof BridgeTransferStatus>
export type BridgeTransfer = z.infer<typeof BridgeTransfer>
export type BridgeTransfers = z.infer<typeof BridgeTransfers>
/** Buyback amounts are base units. Totals may exceed one transaction's u64 range. */
export const BuybackAction = z.enum(['claim', 'swap', 'burn'])
export const BuybackRow = z.strictObject({
  id: z.string(), cycleId: z.string(), action: BuybackAction, signature: Signature,
  lamports: U64, tokenAmount: U64, createdAt: Timestamp,
})
/**
 * Mainnet replay: recorded Solana mainnet blocks run through Acrux's merge and selection
 * (`acrux replay --json`). Transactions are keyed by their base58 signature. An asset is a mint
 * address or "SOL" (lamports and wrapped SOL); amounts are raw integer strings of that asset, signed
 * where a trader or victim can come out behind.
 */
export const SignedUnits = z.string().max(79).regex(/^-?(0|[1-9][0-9]*)$/, 'Must be an integer string.')
export const ReplayAsset = z.union([z.literal('SOL'), Address])
export const ReplayLeaderItem = z.strictObject({ signature: Signature, priorityFeePerCu: Count, computeLimit: Count })
export const ReplayMergedItem = z.strictObject({ signature: Signature, priorityFeePerCu: Count, acruxSlot: Count, track: z.number().int().min(1).max(4), startCu: Count, lengthCu: Count })
export const ReplayRepricing = z.strictObject({
  pool: z.enum(['pump-curve', 'concentrated', 'constant-product']),
  /** What the victim got of the asset it bought, and would have got with no front leg (sent 2-of-3). */
  victimGot: BaseUnits, victimWouldGet: BaseUnits,
  /** The difference valued in the asset the victim paid with, at its own price. */
  keptPaid: SignedUnits,
  /** Sent plain (t = 1): the cost of outbidding the victim's priority fee, and the trader's take after it. */
  outbidLamports: BaseUnits, takeAfterOutbid: SignedUnits,
})
export const ReplaySandwich = z.strictObject({
  victim: Signature, victimPosition: Count, front: Signature, frontPosition: Count, back: Signature, backPosition: Count,
  trader: Address, bought: ReplayAsset, paid: ReplayAsset, vaults: z.array(Address),
  /** What the two legs left the trader in the asset the victim paid with. */
  take: SignedUnits,
  repricing: ReplayRepricing.nullable(),
})
export const ReplaySlotRecord = z.strictObject({
  slot: Count, transactions: Count, votesLeftOut: Count, cuRequested: Count,
  acruxSlots: z.array(z.strictObject({ transactions: Count, cuPerTrack: z.array(Count).length(4) })),
  leaderOrder: z.array(ReplayLeaderItem),
  mergedOrder: z.array(ReplayMergedItem),
  sandwiches: z.array(ReplaySandwich),
})
export const ReplaySandwichRow = ReplaySandwich.extend({ slot: Count })
/**
 * The replayed range with its totals, over every slot: `sandwichCount` counts each front and back
 * pair once, `sandwichesFound` and `sandwichesPriced` count victims; SOL-paid sandwiches sum into
 * the lamport totals. `sandwiches` lists the newest 500 victims only, so totals come from here.
 */
export const ReplayRange = z.strictObject({
  fromSlot: Count.nullable(), toSlot: Count.nullable(), slotsReplayed: Count, transactions: Count, acruxSlots: Count,
  sandwichCount: Count, sandwichesFound: Count, sandwichesPriced: Count, lamportsTaken: SignedUnits, lamportsKept: SignedUnits,
  readAt: Timestamp.nullable(),
  /** Newest first. */
  sandwiches: z.array(ReplaySandwichRow).max(500),
})
export type ReplaySlotRecord = z.infer<typeof ReplaySlotRecord>
export type ReplaySandwich = z.infer<typeof ReplaySandwich>
export type ReplaySandwichRow = z.infer<typeof ReplaySandwichRow>
export type ReplayRange = z.infer<typeof ReplayRange>

export const Buyback = z.strictObject({
  status: z.enum(['waiting', 'live']), mint: Address.nullable(), wallet: Address,
  totals: z.strictObject({ claimedLamports: BaseUnits, swappedLamports: BaseUnits, acruxBought: BaseUnits, acruxBurned: BaseUnits }),
  rows: z.array(BuybackRow).max(50),
})
export type BuybackAction = z.infer<typeof BuybackAction>
export type BuybackRow = z.infer<typeof BuybackRow>
export type Buyback = z.infer<typeof Buyback>
/** A finalized Solana supply observation; sampledAt is when the API read it. */
export const BurnSupply = z.strictObject({ amount: U64, decimals: z.number().int().min(0).max(255), sampledAt: Timestamp })
export const BurnEpoch = z.strictObject({
  epoch: Count, firstSlot: Count, lastSlot: Count,
  startedAt: Timestamp.nullable(), endedAt: Timestamp.nullable(), closed: z.boolean(),
  /** Null means this node has no complete historical observation. */
  baseFeeBurnedLamports: U64.nullable(), feeBurnAmountBaseUnits: U64.nullable(), remainderLamports: U64.nullable(),
  solanaBurn: z.strictObject({ signature: Signature.nullable(), state: z.enum(['not_yet_due', 'pending', 'burned']) }),
  buybackBurnBaseUnits: BaseUnits.nullable(), mintSupplyAtEnd: BurnSupply.nullable(),
})
export const Burn = z.strictObject({
  mint: Address.nullable(), mintSupply: BurnSupply.nullable(),
  totals: z.strictObject({ baseFeeBurnBaseUnits: BaseUnits, buybackBurnBaseUnits: BaseUnits }),
  epochs: page(BurnEpoch),
})
export type BurnSupply = z.infer<typeof BurnSupply>
export type BurnEpoch = z.infer<typeof BurnEpoch>
export type Burn = z.infer<typeof Burn>
export const Health = z.strictObject({ status: z.enum(['fixture', 'waiting', 'ready']), contractVersion: z.string(), missingConfiguration: z.array(z.string()) })
export const SCENARIOS = ['normal', 'empty', 'waiting', 'pending', 'executed', 'finalized', 'skipped', 'sent', 'partly_included', 'failed', 'expired', 'unknown', 'active', 'inactive', 'pending_only', 'queued', 'running', 'done', 'failed_to_start', 'limit_reached', 'unfunded', 'stalled', 'unavailable', 'code_taken', 'crash', 'disagreeing_vectors', 'missing_attestation', 'equivocation', 'majority_lost', 'burn-no-epochs', 'burn-pending', 'burn-burned', 'burn-running'] as const
// Inferred wire types: schema and type share a name without separate interfaces.
export type Slice = z.infer<typeof Slice>
export type CommitmentArrival = z.infer<typeof CommitmentArrival>
export type FragmentInclusion = z.infer<typeof FragmentInclusion>
export type Skip = z.infer<typeof Skip>
export type Counters = z.infer<typeof Counters>
export type Balance = z.infer<typeof Balance>
export type DemoMint = z.infer<typeof DemoMint>
export type DemoPool = z.infer<typeof DemoPool>
export type QuoteRequest = z.infer<typeof QuoteRequest>
export type Quote = z.infer<typeof Quote>
export type PlainSubmission = z.infer<typeof PlainSubmission>
export type FragmentSubmission = z.infer<typeof FragmentSubmission>
export type Submission = z.infer<typeof Submission>
export type Hubs = z.infer<typeof Hubs>
export type RegistryConfig = z.infer<typeof RegistryConfig>
export type SchedulePreview = z.infer<typeof SchedulePreview>
export type FaucetPolicy = z.infer<typeof FaucetPolicy>
export type FaucetStatus = z.infer<typeof FaucetStatus>
export type FaucetRequest = z.infer<typeof FaucetRequest>
export type FaucetGrant = z.infer<typeof FaucetGrant>
export type Facts = z.infer<typeof Facts>
export type ScenarioId = z.infer<typeof ScenarioId>
export type Evidence = z.infer<typeof Evidence>
export type LabScenario = z.infer<typeof LabScenario>
export type LabRunRequest = z.infer<typeof LabRunRequest>
export type LabRuns = z.infer<typeof LabRuns>
export type SpecMap = z.infer<typeof SpecMap>
export type Health = z.infer<typeof Health>
