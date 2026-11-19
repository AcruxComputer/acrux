// The Hub Registry read model, one per chain: Acrux's own registry (read through Acrux's
// Solana-compatible RPC) and the one on Solana mainnet. Each tick (REGISTRY_POLL_SECONDS) it reads the epoch and the program's
// newest signature, two calls; only when either changed does it read the program's accounts on the
// Solana RPC, join them with the stake snapshot and the node's own stake caps (`acrux schedule`, the
// code path the node's schedule runs), and writes hubs and members to Postgres in one transaction.
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { Address, Count, U64, type Hub, type Member, type RegistryConfig } from '@acrux/contract'
import { z } from 'zod'
import { fromBase64, toBase58 } from './engines/core/encoding.js'
import type { PrismaClient } from '@prisma/client'
import { hubStakes } from './schedule.js'

const discriminators = {
  registry: [47, 174, 110, 246, 184, 182, 252, 218],
  hub: [143, 189, 4, 120, 169, 234, 14, 125],
  membership: [231, 141, 180, 98, 109, 168, 175, 166],
}

class Reader {
  private at = 8
  constructor(private bytes: Uint8Array) {}
  key() { const v = toBase58(this.bytes.subarray(this.at, this.at + 32)); this.at += 32; return v }
  u8() { return this.bytes[this.at++]! }
  u16() { const v = this.view().getUint16(this.at, true); this.at += 2; return v }
  u32() { const v = this.view().getUint32(this.at, true); this.at += 4; return v }
  i32() { const v = this.view().getInt32(this.at, true); this.at += 4; return v }
  u64() { const v = this.view().getBigUint64(this.at, true); this.at += 8; return v }
  code() { const v = new TextDecoder().decode(this.bytes.subarray(this.at, this.at + 8)).replace(/\0+$/, ''); this.at += 8; return v }
  private view() { return new DataView(this.bytes.buffer, this.bytes.byteOffset, this.bytes.byteLength) }
}

type ChainRegistry = { depositLamports: bigint; registrationFeeBps: number; cooldownEpochs: bigint; inactivityEpochs: bigint; minMembers: number; codeMinLen: number; codeMaxLen: number }
type ChainHub = { address: string; code: string; x: number; y: number; z: number; registrant: string; depositLamports: bigint; createdEpoch: bigint; lastActiveEpoch: bigint }
type ChainMembership = { address: string; identity: string; hub: string; joinedEpoch: bigint; effectiveEpoch: bigint; pending: 'none' | 'leave' | 'move'; pendingHub: string; pendingEpoch: bigint; cooldownUntilEpoch: bigint }
const ValidatorSet = z.object({ epoch: Count, cutoffSlot: Count.nullable(), catalog: z.array(Address), genesisIdentities: z.array(Address),
  members: z.array(z.object({ identity: Address, hub: z.string().min(1), stake: U64, validatorKind: z.enum(['genesis', 'external']), admittedAtEpoch: Count })) })

function decodeRegistry(b: Uint8Array): ChainRegistry {
  const r = new Reader(b); r.key(); r.key(); r.key()
  return { depositLamports: r.u64(), registrationFeeBps: r.u16(), cooldownEpochs: r.u64(), inactivityEpochs: r.u64(), minMembers: r.u32(), codeMinLen: r.u8(), codeMaxLen: r.u8() }
}
function decodeHub(address: string, b: Uint8Array): ChainHub {
  const r = new Reader(b); r.u64()
  const code = r.code(), x = r.i32(), y = r.i32(), z = r.i32(), registrant = r.key(), depositLamports = r.u64()
  r.u32(); r.u32(); r.u32(); r.u64()
  return { address, code, x, y, z, registrant, depositLamports, createdEpoch: r.u64(), lastActiveEpoch: r.u64() }
}
function decodeMembership(address: string, b: Uint8Array): ChainMembership {
  const r = new Reader(b)
  const identity = r.key(), hub = r.key(); r.u64()
  const joinedEpoch = r.u64(), effectiveEpoch = r.u64(), pending = (['none', 'leave', 'move'] as const)[r.u8()]!
  const pendingHub = r.key(); r.u64()
  return { address, identity, hub, joinedEpoch, effectiveEpoch, pending, pendingHub, pendingEpoch: r.u64(), cooldownUntilEpoch: r.u64() }
}

/** WGS84 ECEF (metres) to geodetic latitude and longitude, Bowring's method. */
export function geodetic(x: number, y: number, z: number) {
  const a = 6_378_137, f = 1 / 298.257223563, b = a * (1 - f), e2 = f * (2 - f), ep2 = (a * a - b * b) / (b * b)
  const p = Math.hypot(x, y), theta = Math.atan2(z * a, p * b)
  const latitude = Math.atan2(z + ep2 * b * Math.sin(theta) ** 3, p - e2 * a * Math.cos(theta) ** 3)
  return { latitude: latitude * 180 / Math.PI, longitude: Math.atan2(y, x) * 180 / Math.PI }
}
/** The regions SIMD-0675 keeps a run inside; the genesis hubs use the same names. */
export function regionOf(latitude: number, longitude: number) {
  if (longitude < -30) return latitude >= 12 ? 'North America' : 'South America'
  if (longitude < 60) return latitude >= 35 ? 'Europe' : 'Africa and Middle East'
  return latitude >= -10 ? 'Asia Pacific' : 'Oceania'
}

export type Chain = 'acrux' | 'solana'
/** Where a chain's stake comes from: the total, and each identity's stake, in lamports. */
export type StakeSource = { refresh(): Promise<void>; total(): bigint; of(identity: string): bigint }

type Snapshot = { totalStakeLamports: string; validators: Record<string, string> }
/** Solana: a mainnet-shaped snapshot; an identity it doesn't list draws 50k to 6.4M SOL from its own hash. */
export function solanaStake(): StakeSource {
  const snapshot: Snapshot = JSON.parse(readFileSync(new URL('../fixtures/stake-snapshot.json', import.meta.url), 'utf8'))
  return {
    refresh: async () => {},
    total: () => BigInt(snapshot.totalStakeLamports),
    of: identity => {
      const listed = snapshot.validators[identity]
      if (listed) return BigInt(listed)
      const u = createHash('sha256').update(identity).digest().readUInt32LE(0) / 2 ** 32
      return BigInt(Math.round(50_000 * 2 ** (u * 7))) * 1_000_000_000n
    },
  }
}
/** Acrux: the stake of the chain's own validators (`getVoteAccounts`); others have none. */
export function acruxStake(gatewayUrl: string): StakeSource {
  let stakes = new Map<string, bigint>()
  return {
    refresh: async () => {
      const response = await fetch(gatewayUrl, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getVoteAccounts', params: [] }), signal: AbortSignal.timeout(5000) })
      const accounts = (await response.json() as { result?: { current: { nodePubkey: string; activatedStake: number }[] } }).result?.current ?? []
      stakes = new Map(accounts.map(a => [a.nodePubkey, BigInt(a.activatedStake)]))
    },
    total: () => [...stakes.values()].reduce((s, v) => s + v, 0n),
    of: identity => stakes.get(identity) ?? 0n,
  }
}
/** The schedule binary takes stakes in units of 1,000 lamports, exact for both chains' stakes. */
const UNIT = 1_000n

export type RegistryState = { config: ChainRegistry | null; epoch: number; slot: number; readAt: number | null; error: string | null }

export class RegistryIndexer {
  state: RegistryState = { config: null, epoch: 0, slot: 0, readAt: null, error: null }
  private timer?: NodeJS.Timeout
  private seen = ''
  constructor(private db: PrismaClient, readonly chain: Chain, private rpcUrl: string, private programId: string, private pollMs: number, readonly stake: StakeSource, private cluster: 'localnet' | 'mainnet-beta' | 'acrux') {}

  start() { const tick = () => this.refresh().catch(e => { this.state.error = String(e?.message ?? e) }).finally(() => { this.timer = setTimeout(tick, this.pollMs) }); tick() }
  stop() { clearTimeout(this.timer) }

  private async rpc<T>(method: string, params: unknown): Promise<T> {
    const response = await fetch(this.rpcUrl, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }), signal: AbortSignal.timeout(5000) })
    const body = await response.json() as { result?: T; error?: { message: string } }
    if (body.error) throw new Error(body.error.message)
    return body.result as T
  }

  async refresh() {
    const [epochInfo, newest] = await Promise.all([
      this.rpc<{ epoch: number; absoluteSlot: number }>('getEpochInfo', [{ commitment: 'confirmed' }]),
      this.rpc<{ signature: string }[]>('getSignaturesForAddress', [this.programId, { limit: 1, commitment: 'confirmed' }]),
    ])
    // A Stake instruction need not touch the registry, so Acrux's live stake is read every tick.
    const seen = `${epochInfo.epoch}:${this.chain === 'acrux' ? epochInfo.absoluteSlot : newest[0]?.signature ?? epochInfo.absoluteSlot}`
    if (seen === this.seen) return
    const [validatorSet] = await Promise.all([
      this.chain === 'acrux' ? this.rpc<unknown>('getValidatorSet', { epoch: epochInfo.epoch }).then(value => ValidatorSet.parse(value)) : Promise.resolve(null),
      this.stake.refresh(),
    ])
    if (validatorSet && validatorSet.epoch !== epochInfo.epoch) throw new Error('The node returned a different validator-set epoch.')
    const admitted = new Map(validatorSet?.members.map(member => [member.identity, member]) ?? [])
    // Legacy and bootstrap epochs retain Round 9's proposer draw, independently of vote weights.
    const frozen = validatorSet !== null && validatorSet.cutoffSlot !== null
    const genesis = new Set(validatorSet?.genesisIdentities ?? [])
    if (validatorSet && (admitted.size !== validatorSet.members.length || validatorSet.members.some(member => member.validatorKind !== (genesis.has(member.identity) ? 'genesis' : 'external')))) throw new Error('The node returned inconsistent validator identities.')
    const stakeOf = (identity: string) => this.stake.of(identity), totalStake = this.stake.total()
    const accounts = await this.rpc<{ pubkey: string; account: { data: [string, string] } }[]>('getProgramAccounts', [this.programId, { encoding: 'base64', commitment: 'confirmed' }])
    const epoch = BigInt(epochInfo.epoch)
    let config: ChainRegistry | null = null
    const hubs: ChainHub[] = [], memberships: ChainMembership[] = []
    for (const { pubkey, account } of accounts) {
      const bytes = fromBase64(account.data[0])
      const tag = [...bytes.subarray(0, 8)].join()
      if (tag === discriminators.registry.join()) config = decodeRegistry(bytes)
      else if (tag === discriminators.hub.join()) hubs.push(decodeHub(pubkey, bytes))
      else if (tag === discriminators.membership.join()) memberships.push(decodeMembership(pubkey, bytes))
    }
    // A membership counts for its hub from its effective epoch until a leave or move takes effect.
    const counted = (m: ChainMembership) => m.effectiveEpoch <= epoch && !(m.pending !== 'none' && m.pendingEpoch <= epoch)
    const byAddress = new Map(hubs.map(h => [h.address, h]))
    const effective = hubs.map(h => ({ code: h.code, region: regionOf(geodetic(h.x, h.y, h.z).latitude, geodetic(h.x, h.y, h.z).longitude),
      members: frozen
        ? validatorSet!.members.filter(m => m.hub === h.code && BigInt(m.stake) > 0n).map(m => ({ identity: m.identity, stake: Number(BigInt(m.stake) / UNIT) }))
        : memberships.filter(m => m.hub === h.address && counted(m) && stakeOf(m.identity) > 0n).map(m => ({ identity: m.identity, stake: Number(stakeOf(m.identity) / UNIT) })) }))
    const caps = new Map((await hubStakes(effective)).map(h => [h.code, h]))
    const minimumStake = totalStake / 100n
    const minMembers = config?.minMembers ?? 4
    await this.db.$transaction(async tx => {
      await tx.member.deleteMany({ where: { chain: this.chain } })
      await tx.hub.deleteMany({ where: { chain: this.chain } })
      for (const h of hubs) {
        const { latitude, longitude } = geodetic(h.x, h.y, h.z)
        const capped = caps.get(h.code)
        const members = effective.find(e => e.code === h.code)!.members
        const proposerStake = capped ? BigInt(capped.stake) * UNIT : members.reduce((s, m) => s + BigInt(m.stake) * UNIT, 0n)
        const active = members.length >= minMembers && proposerStake >= minimumStake
        const lastActive = active ? epoch : h.lastActiveEpoch > h.createdEpoch ? h.lastActiveEpoch : null
        const releasableEpoch = (lastActive ?? h.createdEpoch) + (config?.inactivityEpochs ?? 4n)
        await tx.hub.create({ data: { chain: this.chain, code: h.code, registryAddress: h.address, registrant: h.registrant, ecefX: h.x, ecefY: h.y, ecefZ: h.z, latitude, longitude,
          depositLamports: h.depositLamports.toString(), status: active ? 'active' : epoch >= releasableEpoch ? 'inactive' : 'pending',
          createdEpoch: h.createdEpoch, lastActiveEpoch: lastActive, releasableEpoch, indexedSlot: BigInt(epochInfo.absoluteSlot) } })
      }
      for (const m of memberships) {
        const fixed = frozen ? admitted.get(m.identity) : undefined
        const moved = m.pending === 'move' && m.pendingEpoch <= epoch
        const hub = fixed ? hubs.find(hub => hub.code === fixed.hub) : byAddress.get(moved ? m.pendingHub : m.hub)
        if (!hub) continue
        const capRow = caps.get(hub.code)?.members.find(c => c.identity === m.identity)
        const stake = stakeOf(m.identity)
        await tx.member.create({ data: { chain: this.chain, identity: m.identity, membershipAddress: m.address, hubCode: hub.code, stakeLamports: stake.toString(),
          proposerStakeLamports: capRow ? (BigInt(capRow.proposerStake) * UNIT).toString() : frozen ? '0' : counted(m) ? stake.toString() : '0',
          capped: capRow?.capped ?? false, effectiveEpoch: m.effectiveEpoch, cooldownUntilEpoch: m.cooldownUntilEpoch,
          validatorKind: validatorSet ? genesis.has(m.identity) ? 'genesis' : 'external' : null,
          admittedAtEpoch: admitted.has(m.identity) ? BigInt(admitted.get(m.identity)!.admittedAtEpoch) : null,
          pendingHubCode: m.pending === 'move' ? byAddress.get(m.pendingHub)?.code ?? null : null,
          leavingEpoch: m.pending === 'leave' ? m.pendingEpoch : null, indexedSlot: BigInt(epochInfo.absoluteSlot),
          joinedEpoch: m.joinedEpoch } })
      }
    })
    this.state = { config, epoch: epochInfo.epoch, slot: epochInfo.absoluteSlot, readAt: Date.now(), error: null }
    this.seen = seen
  }

  /** The contract's RegistryConfig from the chain's Registry account. */
  configView(): RegistryConfig {
    const c = this.state.config
    return { status: c ? 'ready' : 'waiting', chain: this.chain, programId: this.programId, cluster: this.cluster,
      depositLamports: c ? c.depositLamports.toString() : null, registrationFeeBps: c?.registrationFeeBps ?? null,
      cooldownEpochs: c ? Number(c.cooldownEpochs) : null, inactivityEpochs: c ? Number(c.inactivityEpochs) : null,
      codeRules: { pattern: c ? `^[A-Z]{${c.codeMinLen},${c.codeMaxLen}}$` : null, minLength: c?.codeMinLen ?? null, maxLength: c?.codeMaxLen ?? null },
      minimumMembers: c?.minMembers ?? null, minimumStake: (this.stake.total() / 100n).toString(), currentEpoch: this.state.epoch,
      missingConfiguration: c ? [] : ['the registry account (initialize has not run on this cluster)'] }
  }
}

type HubRow = Awaited<ReturnType<PrismaClient['hub']['findFirstOrThrow']>> & { members: Awaited<ReturnType<PrismaClient['member']['findFirstOrThrow']>>[] }

/** Rebuilds the contract's Hub from its rows. */
export function hubView(row: HubRow, config: RegistryConfig): Hub {
  const effective = row.members.filter(m => BigInt(m.proposerStakeLamports.toString()) > 0n)
  const stake = effective.reduce((s, m) => s + BigInt(m.stakeLamports.toString()), 0n)
  const proposerStake = effective.reduce((s, m) => s + BigInt(m.proposerStakeLamports.toString()), 0n)
  const cap = effective.reduce((s, m) => (BigInt(m.proposerStakeLamports.toString()) > s ? BigInt(m.proposerStakeLamports.toString()) : s), 0n)
  const minimumStake = BigInt(config.minimumStake ?? '0')
  const epoch = BigInt(config.currentEpoch)
  return {
    chain: row.chain as Chain, code: row.code, name: row.code, region: regionOf(row.latitude, row.longitude),
    ecef: { x: row.ecefX, y: row.ecefY, z: row.ecefZ }, latitude: row.latitude, longitude: row.longitude,
    registrant: row.registrant, depositLamports: row.depositLamports.toString(),
    members: row.members.map(memberView), stake: stake.toString(), proposerStake: proposerStake.toString(), proposerStakeCap: cap.toString(),
    status: row.status,
    needs: { members: Math.max(0, (config.minimumMembers ?? 4) - effective.length), stake: (minimumStake > proposerStake ? minimumStake - proposerStake : 0n).toString() },
    createdEpoch: Number(row.createdEpoch), lastActiveEpoch: row.lastActiveEpoch === null ? null : Number(row.lastActiveEpoch),
    releasableEpoch: row.releasableEpoch === null ? null : Number(row.releasableEpoch),
    releasable: row.status !== 'active' && row.releasableEpoch !== null && epoch >= row.releasableEpoch,
  }
}

export function memberView(m: HubRow['members'][number]): Member {
  return { chain: m.chain as Chain, identity: m.identity, hubCode: m.hubCode, stake: m.stakeLamports.toString(), proposerStake: m.proposerStakeLamports.toString(), capped: m.capped,
    effectiveEpoch: Number(m.effectiveEpoch), joinedEpoch: Number(m.joinedEpoch), pendingMove: m.pendingHubCode,
    pendingLeave: m.leavingEpoch !== null, cooldownUntilEpoch: Number(m.cooldownUntilEpoch),
    ...(m.chain === 'acrux' && m.validatorKind ? { validatorKind: m.validatorKind as 'genesis' | 'external',
      admittedAtEpoch: m.admittedAtEpoch === null ? null : Number(m.admittedAtEpoch) } : {}) }
}
