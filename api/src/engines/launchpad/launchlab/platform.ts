// Running a LaunchLab platform: create the PlatformConfig, change any of its fields, and gate it
// with an allow config (which GlobalConfigs its launches may use) and a curve rule (which launch
// parameters they may pass). Builders only; the admin, or the curve rule manager, signs.

import { PublicKey, SystemProgram, type AccountMeta } from '@solana/web3.js'
import { platformAllowConfigAddress, platformConfigAddress, platformCurveRuleAddress } from './addresses.js'
import { launchlabInstruction } from './codec.js'
import type { ParamConstraint, PlatformConfig } from './accounts.js'

/** LP shares at migration, out of 1,000,000. create_platform_config refuses a nonzero creatorScale. */
export type MigrateNftInfo = { platformScale: bigint; creatorScale: bigint; burnScale: bigint }

export type PlatformParams = {
  admin: PublicKey
  feeWallet: PublicKey
  /** Receives the Fee Key NFT for the locked LP share at every migration. */
  nftWallet: PublicKey
  /** CPMM AmmConfig the platform's pools graduate into. */
  cpswapConfig: PublicKey
  migrateNftInfo: MigrateNftInfo
  /** Platform fee on every curve trade, parts per million; refused above 50,000 (5%). */
  feeRate: bigint
  /** Creator fee on every curve trade, parts per million; refused above 5,000 (0.5%). */
  creatorFeeRate: bigint
  name: string; web: string; img: string
  transferFeeExtensionAuth?: PublicKey
  vestingWallet?: PublicKey
  platformVestingScale?: bigint
}

const nft = (m: MigrateNftInfo) => ({ platform_scale: m.platformScale, creator_scale: m.creatorScale, burn_scale: m.burnScale })

/** The admin pays the PlatformConfig's rent (944 bytes); a PDA admin needs those lamports before it signs. */
export function createPlatformConfigInstruction(p: PlatformParams) {
  return launchlabInstruction('create_platform_config', {
    platform_params: {
      migrate_nft_info: nft(p.migrateNftInfo), fee_rate: p.feeRate, name: p.name, web: p.web, img: p.img,
      creator_fee_rate: p.creatorFeeRate, platform_vesting_scale: p.platformVestingScale ?? 0n,
    },
  }, {
    platform_admin: p.admin, platform_fee_wallet: p.feeWallet, platform_nft_wallet: p.nftWallet,
    platform_config: platformConfigAddress(p.admin), cpswap_config: p.cpswapConfig,
    transfer_fee_extension_authority: p.transferFeeExtensionAuth ?? PublicKey.default,
    platform_vesting_wallet: p.vestingWallet ?? p.admin,
  })
}

/** Every field AllInfo writes; build it from the live config with platformParamsOf, then change what you mean to. */
export type PlatformInfo = Required<Omit<PlatformParams, 'admin'>>

/** One update_platform_config variant. creatorFeeRate and transferFeeExtensionAuth have no variant of their own: they change only through allInfo. */
export type PlatformUpdate =
  | { feeWallet: PublicKey } | { nftWallet: PublicKey } | { migrateNftInfo: MigrateNftInfo } | { feeRate: bigint }
  | { name: string } | { web: string } | { img: string } | { cpswapConfig: PublicKey }
  | { allInfo: PlatformInfo }
  | { vestingWallet: PublicKey } | { platformVestingScale: bigint } | { platformCpCreator: PublicKey }
  | { restrictGlobalConfig: boolean } | { restrictCurveParam: boolean } | { curveRuleManager: PublicKey }

const VARIANT: Record<string, string> = {
  feeWallet: 'FeeWallet', nftWallet: 'NFTWallet', migrateNftInfo: 'MigrateNftInfo', feeRate: 'FeeRate', name: 'Name', web: 'Web', img: 'Img',
  cpswapConfig: 'CpSwapConfig', allInfo: 'AllInfo', vestingWallet: 'VestingWallet', platformVestingScale: 'PlatformVestingScale',
  platformCpCreator: 'PlatformCPCreator', restrictGlobalConfig: 'RestrictGlobalConfig', restrictCurveParam: 'RestrictCurveParam', curveRuleManager: 'CurveRuleManager',
}

/**
 * update_platform_config, signed by the admin. CpSwapConfig carries no payload: the new AmmConfig
 * is the third account, and AllInfo names it there too (AllInfo also sets cpswap_config). The two
 * restrict flags travel as u64 0 or 1.
 */
export function updatePlatformConfigInstruction(admin: PublicKey, update: PlatformUpdate) {
  const [key, value] = Object.entries(update)[0] as [string, unknown]
  let payload: unknown[] = [value]
  let third: PublicKey | undefined
  if (key === 'cpswapConfig') { payload = []; third = value as PublicKey }
  if (key === 'migrateNftInfo') payload = [nft(value as MigrateNftInfo)]
  if (key === 'restrictGlobalConfig' || key === 'restrictCurveParam') payload = [value ? 1n : 0n]
  if (key === 'allInfo') {
    const p = value as PlatformInfo
    third = p.cpswapConfig
    payload = [{
      fee_wallet: p.feeWallet, nft_wallet: p.nftWallet, migrate_nft_info: nft(p.migrateNftInfo), fee_rate: p.feeRate,
      name: p.name, web: p.web, img: p.img, transfer_fee_extension_auth: p.transferFeeExtensionAuth,
      creator_fee_rate: p.creatorFeeRate, platform_vesting_scale: p.platformVestingScale, vesting_wallet: p.vestingWallet,
    }]
  }
  const remaining: AccountMeta[] = third ? [{ pubkey: third, isSigner: false, isWritable: false }] : []
  return launchlabInstruction('update_platform_config', { param: { [VARIANT[key]!]: payload } }, { platform_admin: admin, platform_config: platformConfigAddress(admin) }, remaining)
}

/** The live config as AllInfo fields, so one field can change while every wallet and scale stays as it is. */
export const platformParamsOf = (c: PlatformConfig): PlatformInfo => ({
  feeWallet: c.platformFeeWallet, nftWallet: c.platformNftWallet, cpswapConfig: c.cpswapConfig,
  migrateNftInfo: { platformScale: c.platformScale, creatorScale: c.creatorScale, burnScale: c.burnScale },
  feeRate: c.feeRate, creatorFeeRate: c.creatorFeeRate, name: c.name, web: c.web, img: c.img,
  transferFeeExtensionAuth: c.transferFeeExtensionAuth, vestingWallet: c.platformVestingWallet, platformVestingScale: c.platformVestingScale,
})

const gate = (signer: PublicKey, platformConfig: PublicKey, globalConfig: PublicKey) => ({
  platform_config: platformConfig, global_config: globalConfig, system_program: SystemProgram.programId,
  platform_allow_config: platformAllowConfigAddress(platformConfig, globalConfig),
  platform_curve_rule: platformCurveRuleAddress(platformConfig, globalConfig),
  platform_admin: signer, curve_rule_authority: signer,
})

/** Lets the platform's launches use `globalConfig` once restrictGlobalConfig is on. One per (platform, GlobalConfig); the admin pays 137 bytes of rent. */
export const createAllowConfigInstruction = (admin: PublicKey, globalConfig: PublicKey) =>
  launchlabInstruction('create_platform_allow_config', {}, gate(admin, platformConfigAddress(admin), globalConfig))
export const closeAllowConfigInstruction = (admin: PublicKey, globalConfig: PublicKey) =>
  launchlabInstruction('close_platform_allow_config', {}, gate(admin, platformConfigAddress(admin), globalConfig))

/** Curve-rule constraint fields and operators (LaunchLab's param_field and param_op ids). */
export const RULE_FIELD = {
  curveType: 0, migrateType: 1, migrateCpmmFeeOn: 2, supply: 3, totalBaseSell: 4, totalQuoteFundRaising: 5,
  totalLockedAmount: 6, cliffPeriod: 7, unlockPeriod: 8, baseTokenProgram: 9, transferFeeEnabled: 10,
  transferFeeBasisPoints: 11, transferFeeMaximumFee: 12, sellRate: 13, lockRate: 14, migrateAmount: 15,
  migrateRate: 16, fundRaisingRate: 17, unixTimestamp: 18,
} as const
export const RULE_OP = { eq: 0, gte: 1, lte: 2, neq: 3 } as const

/**
 * Curve rules, one account per (platform, GlobalConfig), signed by the curve rule manager (the
 * admin until changed). A launch passes when every constraint of any one group holds; a range is
 * two constraints in one group. The signer pays rent: 150 bytes plus 14 per group and 18 per constraint.
 */
export const createCurveRuleInstruction = (manager: PublicKey, platformConfig: PublicKey, globalConfig: PublicKey) =>
  launchlabInstruction('create_platform_curve_rule', {}, gate(manager, platformConfig, globalConfig))
export const updateCurveRuleInstruction = (manager: PublicKey, platformConfig: PublicKey, globalConfig: PublicKey, groupId: number, constraints: ParamConstraint[]) =>
  launchlabInstruction('update_platform_curve_rule', { group_id: groupId, constraints }, gate(manager, platformConfig, globalConfig))
export const removeCurveRuleGroupInstruction = (manager: PublicKey, platformConfig: PublicKey, globalConfig: PublicKey, groupId: number) =>
  launchlabInstruction('remove_platform_curve_rule', { group_id: groupId }, gate(manager, platformConfig, globalConfig))
export const closeCurveRuleInstruction = (manager: PublicKey, platformConfig: PublicKey, globalConfig: PublicKey) =>
  launchlabInstruction('close_platform_curve_rule', {}, gate(manager, platformConfig, globalConfig))
