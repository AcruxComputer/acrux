import type { PublicKey, TransactionInstruction } from '@solana/web3.js'

export type ClaimInput = { mint: PublicKey; wallet: PublicKey; tokenProgram: PublicKey }
export type ClaimPlan = { instructions: TransactionInstruction[]; lookupTables?: PublicKey[] }
export type ClaimedAmounts = {
  lamports: bigint
  tokenAmount: bigint
  curveLamports: bigint
  cpmmLamports: bigint
  cpmmTokenAmount: bigint
}
export type ClaimPreview = Pick<ClaimedAmounts, 'curveLamports' | 'cpmmLamports' | 'cpmmTokenAmount'>

/** A launchpad owns instruction assembly and interpretation of its confirmed claim. */
export type LaunchpadAdapter = {
  claimPlan(input: ClaimInput): Promise<ClaimPlan>
  /** Read-only estimates for dry-run quotes; confirmed income always comes from claimedAmounts. */
  previewAmounts(input: ClaimInput): Promise<ClaimPreview>
  claimedAmounts(raw: unknown, input: ClaimInput): ClaimedAmounts
}
