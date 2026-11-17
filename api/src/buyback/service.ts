import { randomUUID } from 'node:crypto'
import type { PrismaClient, BuybackCycle } from '@prisma/client'
import { ComputeBudgetProgram, TransactionMessage, VersionedTransaction, type Connection, type Keypair } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, createBurnInstruction, getAssociatedTokenAddressSync, unpackMint } from '@solana/spl-token'
import { createJournal } from '../engines/keeper/journal.js'
import { prismaKeeperStore } from '../engines/keeper/prisma-store.js'
import { buildUnsignedV0, wireSize, type UnsignedBuild } from '../engines/tx/server/build.js'
import { loadLookupTables } from '../engines/tx/server/lookup-tables.js'
import { connectionRpc, readTransaction } from '../engines/indexer/rpc.js'
import { createPumpfunAdapter } from './pumpfun.js'
import { createLaunchlabAdapter } from './launchlab.js'
import { createBuybackSwapper, type Swapper, type SwapPlan } from './jupiter.js'
import type { BuybackConfig } from './config.js'
import type { ClaimInput, ClaimPlan, ClaimedAmounts, LaunchpadAdapter } from './adapter.js'
import { readBuybackReceipt, receiptTokenDelta, type BuybackReceipt } from './receipt.js'

const BURN_FEE_LAMPORTS = 5_000n // One signer and no priority fee.
const U64_MAX = 18_446_744_073_709_551_615n
const exact = (value: number, label: string) => {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`RPC returned an inexact ${label}.`)
  return BigInt(value)
}

export type BuybackOptions = {
  config: BuybackConfig
  db: PrismaClient
  connection: Connection
  signer: Keypair
  launchpad?: LaunchpadAdapter
  swapper?: Swapper
  isLeader?: () => boolean
  log?: (entry: Record<string, unknown>) => void
}

/** Every transition is persisted after its transaction's facts, so a crash repeats only reads. */
export function createBuybackService(options: BuybackOptions) {
  const { config, db, connection, signer, isLeader = () => true, log = () => {} } = options
  if (!signer.publicKey.equals(config.wallet)) throw new Error('The signer does not match LAUNCH_WALLET.')
  const launchpad = options.launchpad ?? (config.launchpad === 'launchlab' ? createLaunchlabAdapter(connection) : createPumpfunAdapter(connection))
  const swapper = options.swapper ?? createBuybackSwapper(config.jupiterApiKey)
  const store = prismaKeeperStore(db)
  const journal = createJournal({ connection, store, signer, isLeader })
  const own = () => { if (!isLeader()) throw new Error('KEEPER_NOT_LEADER') }
  const actionKey = (cycle: BuybackCycle, action: string) => `buyback:${cycle.id}:${action}`
  const burn = (amount: bigint, input: ClaimInput) => createBurnInstruction(getAssociatedTokenAddressSync(input.mint, input.wallet, false, input.tokenProgram), input.mint, input.wallet, amount, [], input.tokenProgram)

  async function transaction(key: string): Promise<BuybackReceipt> {
    const journaled = await store.get(key)
    if (journaled?.state !== 'confirmed') throw new Error('Buyback transaction is not confirmed.')
    const raw = await readTransaction(connectionRpc(connection), journaled.signature)
    if (!raw) throw new Error('Confirmed buyback transaction is not available yet.')
    return readBuybackReceipt(raw, journaled.signature, config.wallet.toBase58())
  }

  async function record(cycle: BuybackCycle, action: string, receipt: BuybackReceipt, lamports: bigint, tokenAmount: bigint, nextState: string, boughtAmount?: bigint) {
    own()
    await db.$transaction(async dbtx => {
      await dbtx.buybackRow.upsert({
        where: { signature: receipt.transaction.signature }, update: {},
        create: { cycleId: cycle.id, action, signature: receipt.transaction.signature, mint: cycle.mint, wallet: cycle.wallet, lamports: lamports.toString(), tokenAmount: tokenAmount.toString() },
      })
      await dbtx.buybackCycle.update({ where: { id: cycle.id }, data: { state: nextState, ...(boughtAmount === undefined ? {} : { boughtAmount: boughtAmount.toString() }) } })
    })
  }

  async function recordClaim(cycle: BuybackCycle, receipt: BuybackReceipt, claimed: ClaimedAmounts) {
    const share = BigInt(cycle.postGraduationBps)
    const owedLamports = claimed.curveLamports + claimed.cpmmLamports * share / 10_000n
    const claimBurnAmount = claimed.cpmmTokenAmount * share / 10_000n
    own()
    await db.$transaction(async dbtx => {
      await dbtx.buybackRow.create({ data: { cycleId: cycle.id, action: 'claim', signature: receipt.transaction.signature, mint: cycle.mint, wallet: cycle.wallet, lamports: claimed.lamports.toString(), tokenAmount: claimed.tokenAmount.toString() } })
      await dbtx.buybackBudget.upsert({
        where: { mint_wallet: { mint: cycle.mint, wallet: cycle.wallet } },
        create: { mint: cycle.mint, wallet: cycle.wallet, owedLamports: owedLamports.toString() },
        update: { owedLamports: { increment: owedLamports.toString() } },
      })
      await dbtx.buybackCycle.update({ where: { id: cycle.id }, data: { state: 'swap', claimBurnAmount: claimBurnAmount.toString() } })
    })
  }

  async function reserveBudget(cycle: BuybackCycle) {
    own()
    return db.$transaction(async dbtx => {
      const budget = await dbtx.buybackBudget.findUnique({ where: { mint_wallet: { mint: cycle.mint, wallet: cycle.wallet } } })
      const owed = BigInt(budget?.owedLamports.toString() ?? '0')
      if (owed < config.minSwapLamports || owed === 0n) {
        return dbtx.buybackCycle.update({ where: { id: cycle.id }, data: { state: BigInt(cycle.claimBurnAmount.toString()) > 0n ? 'burn' : 'done' } })
      }
      if (owed > U64_MAX) throw new Error('Claim-funded swap budget exceeds a u64.')
      await dbtx.buybackBudget.update({ where: { mint_wallet: { mint: cycle.mint, wallet: cycle.wallet } }, data: { owedLamports: '0' } })
      return dbtx.buybackCycle.update({ where: { id: cycle.id }, data: { swapLamports: owed.toString() } })
    })
  }

  async function preparedWithFloor({ instructions, lookupTables = [] }: ClaimPlan, reservedFees = 0n) {
    const build = await buildUnsignedV0({ connection, payer: config.wallet, instructions, lookupTables: await loadLookupTables(connection, lookupTables), walletSigned: false })
    const simulation = await connection.simulateTransaction(build.transaction, { sigVerify: false, commitment: 'confirmed', accounts: { encoding: 'base64', addresses: [config.wallet.toBase58()] } })
    if (simulation.value.err || !simulation.value.accounts?.[0]) throw new Error('Wallet-balance simulation failed.')
    if (exact(simulation.value.accounts[0].lamports, 'simulated wallet balance') < config.solFloorLamports + reservedFees) throw new Error('Transaction would spend the wallet SOL floor or reserved fees.')
    return build
  }

  async function prepareSwap(lamports: bigint, input: ClaimInput): Promise<{ build: UnsignedBuild; plan: SwapPlan; lamports: bigint }> {
    const plan = await swapper.instructions({ ...input, lamports, slippageBps: config.slippageBps })
    if (plan.minimumOut <= 0n) throw new Error('Swap minimum output must be positive.')
    const build = await preparedWithFloor(plan, BURN_FEE_LAMPORTS)
    return { build, plan, lamports }
  }

  async function dryRun(input: ClaimInput) {
    const claim = await launchpad.claimPlan(input)
    if (claim.instructions.length) {
      const built = await preparedWithFloor(claim)
      log({ event: 'buyback_dry_run', action: 'claim', wireBytes: built.wireBytes, unitsConsumed: built.unitsConsumed })
    } else log({ event: 'buyback_dry_run', action: 'claim', instructions: 0 })
    const where = { mint: input.mint.toBase58(), wallet: input.wallet.toBase58() }
    const [budget, reserved, preview] = await Promise.all([
      db.buybackBudget.findUnique({ where: { mint_wallet: where } }),
      db.buybackCycle.findFirst({ where: { ...where, state: 'swap', swapLamports: { not: null } }, orderBy: { createdAt: 'asc' } }),
      launchpad.previewAmounts(input),
    ])
    const share = BigInt(reserved?.postGraduationBps ?? config.postGraduationBps)
    const previewEntitlement = preview.curveLamports + preview.cpmmLamports * share / 10_000n
    const amount = reserved?.swapLamports === undefined || reserved.swapLamports === null
      ? BigInt(budget?.owedLamports.toString() ?? '0') + previewEntitlement
      : BigInt(reserved.swapLamports.toString())
    const claimedBurn = reserved ? BigInt(reserved.claimBurnAmount.toString()) : preview.cpmmTokenAmount * share / 10_000n
    log({ event: 'buyback_dry_run', action: 'budget', lamports: amount.toString(), claimBurnAmount: claimedBurn.toString(), source: reserved ? 'persisted_reservation' : 'persisted_carry_and_unconfirmed_fee_preview' })
    if (amount === 0n || amount < config.minSwapLamports) {
      log({ event: 'buyback_dry_run', action: 'swap', status: 'below_minimum_claim_budget', owedLamports: amount.toString() })
      if (claimedBurn > 0n) {
        const { blockhash } = await connection.getLatestBlockhash('confirmed')
        const transaction = new VersionedTransaction(new TransactionMessage({ payerKey: config.wallet, recentBlockhash: blockhash, instructions: [burn(claimedBurn, input)] }).compileToV0Message())
        log({ event: 'buyback_dry_run', action: 'burn', wireBytes: wireSize(transaction), amount: claimedBurn.toString(), measurement: 'size_only_from_fee_preview' })
      }
      return
    }
    const plan = await swapper.instructions({ ...input, lamports: amount, slippageBps: config.slippageBps })
    if (plan.minimumOut <= 0n || amount > U64_MAX) throw new Error('Dry-run swap has an invalid amount.')
    const tables = await loadLookupTables(connection, [...new Map([...(claim.lookupTables ?? []), ...plan.lookupTables].map(address => [address.toBase58(), address])).values()])
    const { blockhash } = await connection.getLatestBlockhash('confirmed')
    const compile = (instructions: ClaimPlan['instructions']) => new VersionedTransaction(new TransactionMessage({ payerKey: config.wallet, recentBlockhash: blockhash, instructions }).compileToV0Message(tables))
    const burnAmount = plan.minimumOut + claimedBurn
    const burnInstruction = burn(burnAmount, input)
    const swapTransaction = compile([ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }), ...plan.instructions])
    const burnTransaction = compile([burnInstruction])
    const combined = compile([ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }), ...claim.instructions, ...plan.instructions, burnInstruction])
    const swapBytes = wireSize(swapTransaction)
    log({ event: 'buyback_dry_run', action: 'swap', wireBytes: Number.isFinite(swapBytes) ? swapBytes : '>1232', fitsPacket: swapBytes <= 1232, lamports: amount.toString(), minimumOut: plan.minimumOut.toString(), measurement: 'size_only_before_claim' })
    log({ event: 'buyback_dry_run', action: 'burn', wireBytes: wireSize(burnTransaction), amount: burnAmount.toString(), measurement: 'size_only_before_swap' })
    const combinedBytes = wireSize(combined)
    log({ event: 'buyback_dry_run', action: 'claim_swap_burn', wireBytes: Number.isFinite(combinedBytes) ? combinedBytes : '>1232', fitsPacket: combinedBytes <= 1232, packetLimit: 1232, burnAmount: burnAmount.toString() })
  }

  async function runCycle() {
    if (!config.mint) return // In particular, do not ask RPC whether the mint exists.
    own()
    const mintInfo = await connection.getAccountInfo(config.mint, 'confirmed')
    if (!mintInfo || (!mintInfo.owner.equals(TOKEN_PROGRAM_ID) && !mintInfo.owner.equals(TOKEN_2022_PROGRAM_ID))) throw new Error('ACRUX_MINT must be an SPL Token or Token-2022 mint.')
    unpackMint(config.mint, mintInfo, mintInfo.owner)
    const input: ClaimInput = { mint: config.mint, wallet: config.wallet, tokenProgram: mintInfo.owner }
    if (config.dryRun) return dryRun(input)

    let cycle = await db.buybackCycle.findFirst({ where: { mint: config.mint.toBase58(), wallet: config.wallet.toBase58(), state: { not: 'done' } }, orderBy: { createdAt: 'asc' } })
    if (!cycle) {
      own()
      cycle = await db.buybackCycle.create({ data: { id: randomUUID(), mint: config.mint.toBase58(), wallet: config.wallet.toBase58(), state: 'claim', postGraduationBps: config.postGraduationBps } })
    }
    if (cycle.state === 'claim') {
      const key = actionKey(cycle, 'claim')
      const existing = await store.get(key)
      const plan = !existing || existing.state === 'failed' || existing.state === 'expired' ? await launchpad.claimPlan(input) : { instructions: [] }
      if (existing?.state === 'signed' || existing?.state === 'confirmed' || plan.instructions.length) {
        const prepared = plan.instructions.length ? await preparedWithFloor(plan) : undefined
        if (await journal.execute(key, () => plan.instructions, prepared) !== 'confirmed') throw new Error('Claim is awaiting confirmation.')
        const tx = await transaction(key)
        const claimed = launchpad.claimedAmounts(tx.raw, input)
        if ([claimed.lamports, claimed.tokenAmount, claimed.curveLamports, claimed.cpmmLamports, claimed.cpmmTokenAmount].some(amount => amount < 0n || amount > U64_MAX) || claimed.lamports !== claimed.curveLamports + claimed.cpmmLamports || claimed.tokenAmount !== claimed.cpmmTokenAmount) throw new Error('Claim receipt returned inconsistent source amounts.')
        if (claimed.tokenAmount > 0n && receiptTokenDelta(tx, input) !== claimed.tokenAmount) throw new Error('Claim token delivery differs from its receipt.')
        await recordClaim(cycle, tx, claimed)
      } else {
        own()
        await db.buybackCycle.update({ where: { id: cycle.id }, data: { state: 'swap' } })
      }
      cycle = (await db.buybackCycle.findUniqueOrThrow({ where: { id: cycle.id } }))
    }
    if (cycle.state === 'swap') {
      if (cycle.swapLamports === null) cycle = await reserveBudget(cycle)
    }
    if (cycle.state === 'swap') {
      const key = actionKey(cycle, 'swap')
      const existing = await store.get(key)
      let prepared: Awaited<ReturnType<typeof prepareSwap>> | undefined
      if (!existing || existing.state === 'expired' || existing.state === 'failed') {
        if (!cycle.swapLamports) throw new Error('Swap is missing its reserved claim budget.')
        prepared = await prepareSwap(BigInt(cycle.swapLamports.toString()), input)
        own()
        cycle = await db.buybackCycle.update({ where: { id: cycle.id }, data: { minimumOut: prepared.plan.minimumOut.toString() } })
      }
      if (await journal.execute(key, () => prepared!.plan.instructions, prepared?.build) !== 'confirmed') throw new Error('Swap is awaiting confirmation.')
      const tx = await transaction(key)
      const delivered = receiptTokenDelta(tx, input)
      if (delivered <= 0n || !cycle.minimumOut || delivered < BigInt(cycle.minimumOut.toString()) || !cycle.swapLamports) throw new Error('Swap delivered less than its recorded minimum.')
      await record(cycle, 'swap', tx, BigInt(cycle.swapLamports.toString()), delivered, 'burn', delivered)
      cycle = await db.buybackCycle.findUniqueOrThrow({ where: { id: cycle.id } })
    }
    if (cycle.state === 'burn') {
      const amount = BigInt(cycle.claimBurnAmount.toString()) + BigInt(cycle.boughtAmount?.toString() ?? '0')
      if (amount <= 0n || amount > U64_MAX) throw new Error('Burn is missing a valid delivered amount.')
      const key = actionKey(cycle, 'burn')
      const existing = await store.get(key)
      const instructions = [burn(amount, input)]
      const prepared = !existing || existing.state === 'expired' || existing.state === 'failed' ? await preparedWithFloor({ instructions }) : undefined
      if (await journal.execute(key, () => instructions, prepared) !== 'confirmed') throw new Error('Burn is awaiting confirmation.')
      const tx = await transaction(key)
      if (receiptTokenDelta(tx, input) !== -amount) throw new Error('Burn receipt differs from the delivered amount.')
      await record(cycle, 'burn', tx, 0n, amount, 'done')
    }
  }
  return { runCycle }
}
