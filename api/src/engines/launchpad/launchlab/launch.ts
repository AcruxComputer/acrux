// Launching a coin on LaunchLab: initialize_v2 (classic SPL base mint) and
// initialize_with_token_2022 (Token-2022 base mint, optional transfer fee), on any GlobalConfig,
// so any quote mint and either quote token program. Builders only; the base mint keypair and the
// payer sign.

import { PublicKey, type AccountMeta } from '@solana/web3.js'
import { TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID } from '@solana/spl-token'
import {
  EVENT_AUTHORITY, LAUNCHLAB_PROGRAM_ID, VAULT_AUTHORITY, metadataAddress, platformAllowConfigAddress,
  platformCurveRuleAddress, poolAddress, poolVaultAddress,
} from './addresses.js'
import { launchlabInstruction } from './codec.js'
import type { PlatformConfig } from './accounts.js'
import type { TradePool } from './trade.js'

export type LaunchInput = {
  payer: PublicKey
  /** The pool's creator of record, who claims its creator fees. Any key, a program PDA included; it never signs. Defaults to the payer. */
  creator?: PublicKey
  /** A fresh mint keypair's key; it signs the launch. */
  baseMint: PublicKey
  globalConfig: PublicKey
  /** The GlobalConfig's quote_mint. */
  quoteMint: PublicKey
  quoteTokenProgram?: PublicKey
  platformConfig: PublicKey
  /** The platform's restriction flags (pass the decoded PlatformConfig): restricted platforms need their allow config and curve rule named. */
  restrictions?: Pick<PlatformConfig, 'restrictGlobalConfig' | 'restrictCurveParam'>
  token: { name: string; symbol: string; uri: string; decimals?: number }
  curve: { supply: bigint; totalBaseSell: bigint; totalQuoteFundRaising: bigint }
  vesting?: { totalLockedAmount: bigint; cliffPeriod: bigint; unlockPeriod: bigint }
  /** Where the graduated CPMM pool charges its creator fee: the quote token only, or both tokens. */
  ammCreatorFeeOn?: 'quoteToken' | 'bothToken'
}

/** The pool, its two vaults and the base mint's metadata, all derived from the two mints. */
export function launchAddresses(baseMint: PublicKey, quoteMint: PublicKey) {
  const pool = poolAddress(baseMint, quoteMint)
  return { pool, baseVault: poolVaultAddress(pool, baseMint), quoteVault: poolVaultAddress(pool, quoteMint), metadata: metadataAddress(baseMint) }
}

/**
 * The pool a launch creates, in the shape the trade builders take, so the creator's first buy can
 * follow the launch in the same transaction (it pays the normal fee stack).
 */
export function launchedPool(input: LaunchInput, opts: { token2022?: boolean } = {}): TradePool {
  const a = launchAddresses(input.baseMint, input.quoteMint)
  const quote2022 = (input.quoteTokenProgram ?? TOKEN_PROGRAM_ID).equals(TOKEN_2022_PROGRAM_ID)
  return {
    address: a.pool, globalConfig: input.globalConfig, platformConfig: input.platformConfig, baseMint: input.baseMint, quoteMint: input.quoteMint,
    baseVault: a.baseVault, quoteVault: a.quoteVault, creator: input.creator ?? input.payer, tokenProgramFlag: (opts.token2022 ? 1 : 0) | (quote2022 ? 2 : 0),
  }
}

const variant = (name: string) => ({ [name.charAt(0).toUpperCase() + name.slice(1)]: {} })

function args(input: LaunchInput) {
  return {
    base_mint_param: { decimals: input.token.decimals ?? 6, name: input.token.name, symbol: input.token.symbol, uri: input.token.uri },
    // Every new launch graduates to CPMM: migrate_type 1. Only constant product (curve type 0) is live.
    curve_param: { Constant: { data: { supply: input.curve.supply, total_base_sell: input.curve.totalBaseSell, total_quote_fund_raising: input.curve.totalQuoteFundRaising, migrate_type: 1 } } },
    vesting_param: {
      total_locked_amount: input.vesting?.totalLockedAmount ?? 0n,
      cliff_period: input.vesting?.cliffPeriod ?? 0n,
      unlock_period: input.vesting?.unlockPeriod ?? 0n,
    },
    amm_fee_on: variant(input.ammCreatorFeeOn ?? 'quoteToken'),
  }
}

function accounts(input: LaunchInput) {
  const a = launchAddresses(input.baseMint, input.quoteMint)
  return {
    payer: input.payer, creator: input.creator ?? input.payer, global_config: input.globalConfig, platform_config: input.platformConfig,
    authority: VAULT_AUTHORITY, pool_state: a.pool, base_mint: input.baseMint, quote_mint: input.quoteMint,
    base_vault: a.baseVault, quote_vault: a.quoteVault, metadata_account: a.metadata,
    quote_token_program: input.quoteTokenProgram ?? TOKEN_PROGRAM_ID, event_authority: EVENT_AUTHORITY, program: LAUNCHLAB_PROGRAM_ID,
  }
}

/** A restricted platform's launch names its allow config and curve rule after the IDL accounts, read-only. */
function restricted(input: LaunchInput): AccountMeta[] {
  const r = input.restrictions
  const out: AccountMeta[] = []
  if (r?.restrictGlobalConfig) out.push({ pubkey: platformAllowConfigAddress(input.platformConfig, input.globalConfig), isSigner: false, isWritable: false })
  if (r?.restrictCurveParam) out.push({ pubkey: platformCurveRuleAddress(input.platformConfig, input.globalConfig), isSigner: false, isWritable: false })
  return out
}

/** initialize_v2: a classic SPL base mint with Metaplex metadata. LaunchLab mints the whole supply to the base vault and revokes the mint authority. */
export const initializeV2Instruction = (input: LaunchInput) => launchlabInstruction('initialize_v2', args(input), accounts(input), restricted(input))

/** initialize_with_token_2022: a Token-2022 base mint with metadata in the mint, optionally a transfer fee (basis points ≤ 500). */
export function initializeWithToken2022Instruction(input: LaunchInput & { transferFee?: { basisPoints: number; maximumFee: bigint } }) {
  const fee = input.transferFee ? { transfer_fee_basis_points: input.transferFee.basisPoints, maximum_fee: input.transferFee.maximumFee } : null
  return launchlabInstruction('initialize_with_token_2022', { ...args(input), transfer_fee_extension_param: fee }, accounts(input), restricted(input))
}
