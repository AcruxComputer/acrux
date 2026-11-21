// The genesis validators' share of stake per bridge epoch, read from the set the node fixed for that
// epoch (`getValidatorSet`). Epochs up to the current one are read once and kept; the next one is
// read each time, since the node can answer it before its set is fixed from finalized state.
import { Address, Count, U64, type GenesisShare } from '@acrux/contract'
import { z } from 'zod'

const ValidatorSet = z.object({ epoch: Count, cutoffSlot: Count.nullable(), genesisIdentities: z.array(Address),
  members: z.array(z.object({ identity: Address, stake: U64 })), genesisWeight: U64.nullable().optional(), genesisStepFromSlot: Count.nullable().optional() })
type Row = GenesisShare['epochs'][number]

export function createGenesisShareReader(rpcUrl: string) {
  const fixed = new Map<number, Row>()
  let stepFromSlot: number | null = null
  const rpc = async (method: string, params: unknown) => {
    const response = await fetch(rpcUrl, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }), signal: AbortSignal.timeout(5000) })
    return await response.json() as { result?: unknown; error?: { data?: { error?: { code?: string } } } }
  }
  const readEpoch = async (epoch: number): Promise<Row | undefined> => {
    const body = await rpc('getValidatorSet', { epoch })
    // The next epoch's set exists only once it is fixed; until then it is left out.
    if (body.error?.data?.error?.code === 'NET_UNAVAILABLE') return
    if (body.error) throw new Error(`getValidatorSet ${epoch} failed`)
    const set = ValidatorSet.parse(body.result)
    stepFromSlot = set.genesisStepFromSlot ?? null
    const genesis = new Set(set.genesisIdentities)
    const staked = set.members.filter(member => BigInt(member.stake) > 0n)
    const genesisStake = staked.filter(member => genesis.has(member.identity)).reduce((sum, member) => sum + BigInt(member.stake), 0n)
    const totalStake = staked.reduce((sum, member) => sum + BigInt(member.stake), 0n)
    return { epoch, fixedAtSlot: set.cutoffSlot, genesisStake: genesisStake.toString(), totalStake: totalStake.toString(),
      genesisShareBps: totalStake === 0n ? 10000 : Number(genesisStake * 10000n / totalStake),
      genesisMembers: staked.filter(member => genesis.has(member.identity)).length, externalMembers: staked.filter(member => !genesis.has(member.identity)).length,
      genesisWeight: set.genesisWeight ?? null }
  }
  return async (): Promise<GenesisShare> => {
    const info = await rpc('getEpochInfo', [{ commitment: 'finalized' }])
    const { epoch } = z.object({ epoch: Count }).parse(info.result)
    const missing = Array.from({ length: epoch + 1 }, (_, at) => at).filter(at => !fixed.has(at))
    const [next] = await Promise.all([readEpoch(epoch + 1), ...missing.map(async at => { const row = await readEpoch(at); if (row) fixed.set(at, row) })])
    const epochs = [...fixed.values()].filter(row => row.epoch <= epoch).sort((a, b) => a.epoch - b.epoch)
    return { currentEpoch: epoch, genesisStepFromSlot: stepFromSlot, epochs: next ? [...epochs, next] : epochs }
  }
}
