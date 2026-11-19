// Native fee burns, the relayer's finalized Solana burns, creator-fee buybacks and supply.
// Historical supply is not reconstructed: a closed epoch keeps its first observed supply quote.
import { ACRUX_BRIDGE_IDL, PageQuery, U64, rpcMethods, type Burn, type BurnEpoch } from '@acrux/contract'
import { BorshInstructionCoder, type Idl } from '@coral-xyz/anchor'
import { PublicKey, VersionedTransaction } from '@solana/web3.js'
import type { PrismaClient } from '@prisma/client'
import { z } from 'zod'
import type BN from 'bn.js'
import { readBuyback } from './buyback/read.js'
import type { Rpc } from './engines/rpc/client.js'
import type { createApi } from './engines/server/app.js'
import { fail } from './engines/server/errors.js'
import { readQuery } from './engines/server/input.js'
import { signatureOf } from './engines/tx/server/ed25519.js'

type Supply = NonNullable<Burn['mintSupply']>
const NodeBurns = rpcMethods.getFeeBurns.result
const TokenSupply = z.object({ value: z.object({ amount: U64, decimals: z.number().int().min(0).max(255) }) })
const coder = new BorshInstructionCoder(ACRUX_BRIDGE_IDL as unknown as Idl)

/** Read the node ledger; the contract checks its shape before it is joined to local records. */
export async function readNodeFeeBurns(url: string, query: PageQuery): Promise<unknown> {
  const response = await fetch(url, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getFeeBurns', params: query }), signal: AbortSignal.timeout(10_000),
  }).catch(() => fail('NET_UNAVAILABLE', 'Acrux fee burns are unavailable.', { retryable: true, stage: 'read' }))
  if (!response.ok) fail('NET_UNAVAILABLE', 'Acrux fee burns are unavailable.', { retryable: true, stage: 'read' })
  const body = await response.json().catch(() => null) as { error?: unknown; result?: unknown } | null
  if (!body || body.error || !Object.hasOwn(body, 'result')) fail('NET_UNAVAILABLE', 'Acrux fee burns are unavailable.', { retryable: true, stage: 'read' })
  return body.result
}

/** A fee-burn journal row contains one exact signed burn instruction; its amount is not inferred
 * from a page's current node ledger. Confirmed fee-burn rows are retained by the keeper. */
function journalBurn(row: { key: string; signature: string; signedBase64: string }, program: PublicKey, mint: PublicKey) {
  const epochText = row.key.slice(`bridge:${program.toBase58()}:fee-burn:`.length)
  if (!/^(0|[1-9]\d*)$/.test(epochText) || !Number.isSafeInteger(Number(epochText))) throw new Error('Invalid fee-burn journal epoch.')
  const transaction = VersionedTransaction.deserialize(Buffer.from(row.signedBase64, 'base64'))
  if (signatureOf(transaction) !== row.signature) throw new Error('Fee-burn journal signature differs from its signed transaction.')
  const burns = transaction.message.compiledInstructions.flatMap(instruction => {
    if (!transaction.message.staticAccountKeys[instruction.programIdIndex]?.equals(program)) return []
    const decoded = coder.decode(Buffer.from(instruction.data))
    if (decoded?.name !== 'burn') return []
    // BurnFees' mint is account four; journaled keeper burns use no address lookup tables.
    if (!transaction.message.staticAccountKeys[instruction.accountKeyIndexes[4]!]?.equals(mint)) throw new Error('Fee-burn journal names another mint.')
    return [decoded.data as { epoch: BN; amount: BN }]
  })
  if (burns.length !== 1 || burns[0]!.epoch.toString() !== epochText) throw new Error('Fee-burn journal does not contain its named burn.')
  const amount = U64.parse(burns[0]!.amount.toString())
  if (amount === '0') throw new Error('Fee-burn journal contains a zero burn.')
  return { epoch: Number(epochText), amount: BigInt(amount) }
}

export function createBurnReader(options: {
  db: PrismaClient
  gateway: (query: PageQuery) => Promise<unknown>
  solanaRpc: Pick<Rpc, 'call'>
  mint: string
  wallet: string
  program: string
}) {
  const { db, gateway, solanaRpc, mint, wallet, program } = options
  let quote: Supply | null = null
  let quoteAt = 0
  let inFlight: Promise<Supply> | null = null
  async function supply(notBefore = 0): Promise<Supply> {
    if (quote && Date.now() - quoteAt < 60_000 && quoteAt >= notBefore) return quote
    if (!inFlight) {
      inFlight = (async () => {
        const parsed = TokenSupply.safeParse(await solanaRpc.call<unknown>('getTokenSupply', [mint, { commitment: 'finalized' }]))
        if (!parsed.success) fail('NET_UNAVAILABLE', 'Solana returned an invalid mint supply.', { retryable: true, stage: 'read' })
        quoteAt = Date.now()
        quote = { ...parsed.data.value, sampledAt: new Date(quoteAt).toISOString() }
        return quote
      })().finally(() => { inFlight = null })
    }
    return inFlight
  }
  async function read(query: PageQuery): Promise<Burn> {
    if (!mint) return { mint: null, mintSupply: null, totals: { baseFeeBurnBaseUnits: '0', buybackBurnBaseUnits: '0' }, epochs: { items: [], page: { nextCursor: null, hasMore: false }, asOf: new Date().toISOString() } }
    const parsed = NodeBurns.safeParse(await gateway(query))
    if (!parsed.success) fail('NET_UNAVAILABLE', 'Acrux returned an invalid fee-burn ledger.', { retryable: true, stage: 'read' })
    const node = parsed.data
    if (node.mint && node.mint !== mint || node.program && program && node.program !== program) fail('CONFIGURATION_REQUIRED', "The burn reader must use the node's native mint and bridge program.", { stage: 'read' })
    if (node.items.length && (!program || !node.program)) fail('CONFIGURATION_REQUIRED', 'Configure the bridge program before reading native burn epochs.', { stage: 'read' })
    if (node.items.length > query.limit || node.items.some((item, index) => item.firstSlot > item.lastSlot || index > 0 && item.epoch >= node.items[index - 1]!.epoch || query.cursor !== undefined && BigInt(item.epoch) >= BigInt(query.cursor))) fail('NET_UNAVAILABLE', 'Acrux returned an invalid fee-burn page.', { retryable: true, stage: 'read' })
    const observedAt = Date.now()
    if (node.items.some(epoch => !epoch.closed && epoch.endedAt !== null
      || epoch.startedAt !== null && epoch.startedAt > observedAt || epoch.endedAt !== null && epoch.endedAt > observedAt
      || epoch.startedAt !== null && epoch.endedAt !== null && epoch.startedAt > epoch.endedAt)) fail('NET_UNAVAILABLE', 'Acrux returned an invalid burn epoch time range.', { retryable: true, stage: 'read' })
    const closed = node.items.filter(epoch => epoch.closed).map(epoch => BigInt(epoch.epoch))
    const [existing, journals, buyback] = await Promise.all([
      closed.length ? db.burnSupplySample.findMany({ where: { program, mint, epoch: { in: closed } } }) : [],
      program ? db.keeperTransaction.findMany({ where: { key: { startsWith: `bridge:${program}:fee-burn:` } }, select: { key: true, state: true, signature: true, signedBase64: true } }) : [],
      readBuyback(db, { mint, wallet }),
    ])
    const byEpoch = new Map(existing.map(sample => [Number(sample.epoch), sample]))
    const missing = node.items.filter(epoch => epoch.closed && !byEpoch.has(epoch.epoch))
    // A quote captured before a known closure cannot become that epoch's first end sample.
    // Unknown historical UTC boundaries use the ordinary minute quote, with its actual timestamp.
    const notBefore = Math.max(0, ...missing.map(epoch => epoch.endedAt ?? 0))
    const currentSupply = await supply(notBefore)
    if (Date.parse(currentSupply.sampledAt) < notBefore) fail('NET_UNAVAILABLE', 'The supply quote predates the closed epoch.', { retryable: true, stage: 'read' })
    if (missing.length) {
      await db.burnSupplySample.createMany({ data: missing.map(epoch => ({ program, mint, epoch: BigInt(epoch.epoch), amount: currentSupply.amount, decimals: currentSupply.decimals, sampledAt: new Date(currentSupply.sampledAt) })), skipDuplicates: true })
      // The database's first writer wins across API processes; always return that immutable quote.
      for (const sample of await db.burnSupplySample.findMany({ where: { program, mint, epoch: { in: closed } } })) byEpoch.set(Number(sample.epoch), sample)
    }
    const asOf = new Date()
    const intervals = node.items.map(epoch => ({
      epoch: epoch.epoch,
      from: epoch.startedAt === null ? null : new Date(epoch.startedAt),
      to: epoch.closed ? epoch.endedAt === null ? null : new Date(epoch.endedAt) : asOf,
    }))
    const known = intervals.filter((interval): interval is { epoch: number; from: Date; to: Date } => interval.from !== null && interval.to !== null)
    const burns = known.length ? await db.buybackRow.findMany({
      where: { mint, wallet, action: 'burn', createdAt: { gte: new Date(Math.min(...known.map(interval => interval.from.getTime()))), lt: new Date(Math.max(...known.map(interval => interval.to.getTime()))) } },
      select: { tokenAmount: true, createdAt: true },
    }) : []
    const prefix = `bridge:${program}:fee-burn:`
    const journalByEpoch = new Map(journals.map(row => [row.key.slice(prefix.length), row]))
    const programKey = program ? new PublicKey(program) : null, mintKey = new PublicKey(mint)
    let confirmedBaseUnits = 0n
    for (const row of journals) if (row.state === 'confirmed') confirmedBaseUnits += journalBurn(row, programKey!, mintKey).amount
    const items = node.items.map((epoch): BurnEpoch => {
      const journal = journalByEpoch.get(String(epoch.epoch))
      // A closed epoch with an unavailable amount is pending evidence, not proof of no burn.
      const due = epoch.closed && epoch.feeBurnAmountBaseUnits !== '0'
      const interval = known.find(interval => interval.epoch === epoch.epoch)
      const creatorBurn = interval ? burns.filter(row => row.createdAt >= interval.from && row.createdAt < interval.to).reduce((sum, row) => sum + BigInt(row.tokenAmount.toFixed(0)), 0n).toString() : null
      const sample = byEpoch.get(epoch.epoch)
      return {
        ...epoch, startedAt: epoch.startedAt === null ? null : new Date(epoch.startedAt).toISOString(), endedAt: epoch.endedAt === null ? null : new Date(epoch.endedAt).toISOString(),
        solanaBurn: { signature: journal?.signature ?? null, state: journal?.state === 'confirmed' ? 'burned' : journal || due ? 'pending' : 'not_yet_due' },
        buybackBurnBaseUnits: creatorBurn,
        mintSupplyAtEnd: sample ? { amount: sample.amount.toFixed(0), decimals: sample.decimals, sampledAt: sample.sampledAt.toISOString() } : null,
      }
    })
    return { mint, mintSupply: currentSupply, totals: { baseFeeBurnBaseUnits: confirmedBaseUnits.toString(), buybackBurnBaseUnits: buyback.totals.acruxBurned }, epochs: { items, page: { nextCursor: node.nextCursor, hasMore: node.nextCursor !== null }, asOf: asOf.toISOString() } }
  }
  return { read }
}

/** Shared production registration: the API check mounts this exact handler without indexers. */
export function mountBurnRoutes(app: ReturnType<typeof createApi>, reader: ReturnType<typeof createBurnReader>) {
  app.get('/v1/burn', async c => c.json(await reader.read(readQuery(c, PageQuery))))
}
