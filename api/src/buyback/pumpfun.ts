import { createRequire } from 'node:module'
import type { Idl } from '@coral-xyz/anchor'
import type { Connection, PublicKey, TransactionInstruction } from '@solana/web3.js'
import { PublicKey as SolanaPublicKey } from '@solana/web3.js'
import {
  createAssociatedTokenAccountIdempotentInstruction, getAssociatedTokenAddressSync,
  NATIVE_MINT, TOKEN_PROGRAM_ID, unpackAccount,
} from '@solana/spl-token'
import { anchorProgram } from '../engines/indexer/anchor.js'
import { decodeTransaction } from '../engines/indexer/decoder.js'
import { readBuybackReceipt, receiptNativeClaimDelta } from './receipt.js'
import type { ClaimInput, ClaimPlan, ClaimPreview, LaunchpadAdapter } from './adapter.js'

// The SDK's ESM entry imports Anchor's CommonJS exports by name.
const require = createRequire(import.meta.url)
const pump: typeof import('@pump-fun/pump-sdk') = require('@pump-fun/pump-sdk')
const amm: typeof import('@pump-fun/pump-swap-sdk') = require('@pump-fun/pump-swap-sdk')
const program = anchorProgram(pump.pumpIdl as Idl)

/** Offline instruction assembly is also used to discover the local validator's clone set. */
export async function buildPumpfunClaimInstructions({ mint, wallet, curveFees, poolFees, ammVaultFunded }: {
  mint: PublicKey
  wallet: PublicKey
  curveFees: boolean
  poolFees: boolean
  ammVaultFunded: boolean
}): Promise<TransactionInstruction[]> {
  const instructions: TransactionInstruction[] = []
  if (curveFees) instructions.push(await pump.PUMP_SDK.sweepCreatorFeeInstruction({
    payer: wallet, mint, creator: wallet, quoteMint: NATIVE_MINT, quoteTokenProgram: TOKEN_PROGRAM_ID,
  }))
  if (poolFees || ammVaultFunded) {
    const vault = amm.coinCreatorVaultAuthorityPda(wallet)
    const ata = getAssociatedTokenAddressSync(NATIVE_MINT, vault, true, TOKEN_PROGRAM_ID)
    // transfer_creator_fees_to_pump_v2 requires this ATA even on a SOL quote.
    instructions.push(createAssociatedTokenAccountIdempotentInstruction(wallet, ata, vault, NATIVE_MINT))
    if (poolFees) instructions.push(await pump.PUMP_SDK.sweepPoolCreatorFeeInstruction({
      payer: wallet, mint, coinCreator: wallet, quoteMint: NATIVE_MINT, quoteTokenProgram: TOKEN_PROGRAM_ID,
    }))
    instructions.push(await amm.PUMP_AMM_SDK.transferCreatorFeesToPumpV2Instruction({
      payer: wallet, coinCreator: wallet, quoteMint: NATIVE_MINT, quoteTokenProgram: TOKEN_PROGRAM_ID,
    }))
  }
  // All addresses are supplied or PDA-derived; the offline SDK never resolves an account over RPC.
  const program = pump.getPumpProgram(null as unknown as Connection)
  instructions.push(await program.methods.collectCreatorFeeV2().accountsPartial({
    creator: wallet,
    creatorTokenAccount: getAssociatedTokenAddressSync(NATIVE_MINT, wallet, true, TOKEN_PROGRAM_ID),
    creatorVault: pump.creatorVaultPda(wallet),
    creatorVaultTokenAccount: getAssociatedTokenAddressSync(NATIVE_MINT, pump.creatorVaultPda(wallet), true, TOKEN_PROGRAM_ID),
    quoteMint: NATIVE_MINT,
    quoteTokenProgram: TOKEN_PROGRAM_ID,
  }).instruction())
  return instructions
}

export function createPumpfunAdapter(connection: Connection): LaunchpadAdapter {
  async function claimPlan({ mint, wallet }: ClaimInput): Promise<ClaimPlan & { preview: ClaimPreview }> {
    const curveAddress = pump.bondingCurvePda(mint)
    const poolAddress = amm.canonicalPumpPoolPda(mint)
    const vaultAddress = amm.coinCreatorVaultAuthorityPda(wallet)
    const vaultAta = getAssociatedTokenAddressSync(NATIVE_MINT, vaultAddress, true, TOKEN_PROGRAM_ID)
    const [curveAccount, poolAccount, vaultAccount, pumpVault] = await connection.getMultipleAccountsInfo(
      [curveAddress, poolAddress, vaultAta, pump.creatorVaultPda(wallet)], 'confirmed',
    )
    if (!curveAccount || !curveAccount.owner.equals(pump.PUMP_PROGRAM_ID)) throw new Error('Invalid pump.fun bonding curve owner')
    const curve = pump.PUMP_SDK.decodeBondingCurve(curveAccount)
    if (!curve.creator.equals(wallet)) throw new Error('The launch wallet is not this pump.fun coin creator')
    if (!curve.quoteMint.equals(SolanaPublicKey.default) && !curve.quoteMint.equals(NATIVE_MINT)) {
      throw new Error('Buyback requires a SOL-quoted pump.fun coin')
    }
    let poolFees = false
    let poolFeeAmount = 0n
    if (poolAccount) {
      if (!poolAccount.owner.equals(amm.PUMP_AMM_PROGRAM_ID)) throw new Error('Invalid PumpSwap pool owner')
      const pool = amm.PUMP_AMM_SDK.decodePool(poolAccount)
      if (!pool.baseMint.equals(mint) || !pool.quoteMint.equals(NATIVE_MINT) || !pool.coinCreator.equals(wallet)) {
        throw new Error('PumpSwap pool does not belong to this mint and launch wallet')
      }
      poolFees = !pool.creatorFees.isZero()
      poolFeeAmount = BigInt(pool.creatorFees.toString())
    }
    const ammVaultAmount = vaultAccount ? unpackAccount(vaultAta, vaultAccount, TOKEN_PROGRAM_ID).amount : 0n
    const ammVaultFunded = ammVaultAmount > 0n
    if (pumpVault && !pumpVault.owner.equals(pump.PUMP_PROGRAM_ID)) throw new Error('Invalid pump.fun creator vault owner')
    const curveFees = !curve.creatorFee.isZero()
    const vaultRent = pumpVault ? await connection.getMinimumBalanceForRentExemption(pumpVault.data.length, 'confirmed') : 0
    if (!Number.isSafeInteger(vaultRent) || vaultRent < 0 || (pumpVault && (!Number.isSafeInteger(pumpVault.lamports) || pumpVault.lamports < 0))) throw new Error('RPC returned inexact creator vault lamports')
    const pumpVaultFunded = pumpVault !== null && BigInt(pumpVault.lamports) > BigInt(vaultRent)
    const vaultFees = pumpVaultFunded ? BigInt(pumpVault!.lamports) - BigInt(vaultRent) : 0n
    const preview = { curveLamports: BigInt(curve.creatorFee.toString()) + poolFeeAmount + ammVaultAmount + vaultFees, cpmmLamports: 0n, cpmmTokenAmount: 0n }
    if (!curveFees && !poolFees && !ammVaultFunded && !pumpVaultFunded) return { instructions: [], preview }
    return { instructions: await buildPumpfunClaimInstructions({ mint, wallet, curveFees, poolFees, ammVaultFunded }), preview }
  }
  return {
    claimPlan,
    previewAmounts: async input => (await claimPlan(input)).preview,
    claimedAmounts(raw, input) {
      const { wallet } = input
      const tx = decodeTransaction(raw, [program])
      const events = tx.events.filter(event => event.programId === program.programId && event.name === 'CollectCreatorFeeEvent')
      if (!tx.valid || tx.failed || events.length !== 1 || events[0].data.creator !== wallet.toBase58() || ![SolanaPublicKey.default.toBase58(), NATIVE_MINT.toBase58()].includes(String(events[0].data.quote_mint))) throw new Error('Claim receipt has no matching native creator-fee event.')
      const amount = events[0].data.creator_fee
      if (typeof amount !== 'string' || !/^\d+$/.test(amount)) throw new Error('Invalid creator-fee amount.')
      const lamports = receiptNativeClaimDelta(readBuybackReceipt(raw, tx.signature, wallet.toBase58()), input)
      if (lamports !== BigInt(amount)) throw new Error('Claim wallet income differs from its pump.fun creator-fee event.')
      return { lamports, tokenAmount: 0n, curveLamports: lamports, cpmmLamports: 0n, cpmmTokenAmount: 0n }
    },
  }
}
