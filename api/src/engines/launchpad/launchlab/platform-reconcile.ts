// Bringing a LaunchLab platform to a spec and keeping it there: compares the live PlatformConfig,
// allow configs and curve rules with what the platform should be, and returns the instructions
// that close the gap. Run it to a fixed point: send what it returns, read again, call again,
// until it returns nothing. Every step is safe to repeat.

import { PublicKey, type Connection, type TransactionInstruction } from '@solana/web3.js'
import { LAUNCHLAB_PROGRAM_ID, platformConfigAddress, platformCurveRuleAddress } from './addresses.js'
import { accountDiscriminator } from './codec.js'
import { toBase58 } from '../../core/encoding.js'
import { decodePlatformAllowConfig, decodePlatformConfig, decodePlatformCurveRule, type ParamConstraint, type PlatformConfig, type PlatformCurveRule } from './accounts.js'
import {
  closeAllowConfigInstruction, createAllowConfigInstruction, createCurveRuleInstruction, createPlatformConfigInstruction,
  removeCurveRuleGroupInstruction, updateCurveRuleInstruction, updatePlatformConfigInstruction, type PlatformParams,
} from './platform.js'

export type PlatformSpec = Required<PlatformParams> & {
  /** CPMM creator of graduated pools; the default key leaves it to each launch's creator. */
  platformCpCreator: PublicKey
  curveRuleManager: PublicKey
  /** GlobalConfigs the platform's launches may use. Allow configs for any other are closed. */
  globalConfigs: { address: PublicKey; ruleGroups?: ParamConstraint[][] }[]
  restrictGlobalConfig: boolean
  restrictCurveParam: boolean
}

export type PlatformLive = {
  config: PlatformConfig | null
  /** GlobalConfigs that have an allow config for this platform. */
  allowed: PublicKey[]
  /** Curve rules by GlobalConfig (base58), null where none exists. */
  rules: Map<string, PlatformCurveRule | null>
}

/** Reads everything the plan compares: the config, every allow config of the platform, the spec's curve rules. */
export async function readPlatformLive(connection: Connection, admin: PublicKey, globalConfigs: PublicKey[]): Promise<PlatformLive> {
  const platform = platformConfigAddress(admin)
  const [allowRows, infos] = await Promise.all([
    connection.getProgramAccounts(LAUNCHLAB_PROGRAM_ID, { commitment: 'confirmed', filters: [
      { memcmp: { offset: 0, bytes: toBase58(accountDiscriminator('PlatformAllowConfig')) } },
      { memcmp: { offset: 9, bytes: platform.toBase58() } },
    ] }),
    connection.getMultipleAccountsInfo([platform, ...globalConfigs.map((g) => platformCurveRuleAddress(platform, g))], 'confirmed'),
  ])
  return {
    config: infos[0] ? decodePlatformConfig(infos[0]) : null,
    allowed: allowRows.map((r) => decodePlatformAllowConfig(r.account).globalConfig),
    rules: new Map(globalConfigs.map((g, i) => [g.toBase58(), infos[i + 1] ? decodePlatformCurveRule(infos[i + 1]) : null])),
  }
}

const sameKey = (a: PublicKey, b: PublicKey) => a.equals(b)
const sameGroup = (a: ParamConstraint[], b: ParamConstraint[]) =>
  a.length === b.length && a.every((c, i) => c.field === b[i]!.field && c.op === b[i]!.op && c.value === b[i]!.value)

/** The next instructions that move the platform toward `spec`; empty once it matches. */
export function platformPlan(spec: PlatformSpec, live: PlatformLive): TransactionInstruction[] {
  const c = live.config
  // The config has to exist before anything else can be compared with it.
  if (!c) return [createPlatformConfigInstruction(spec)]
  const out: TransactionInstruction[] = []
  const admin = spec.admin, platform = platformConfigAddress(admin)
  const update = (u: Parameters<typeof updatePlatformConfigInstruction>[1]) => out.push(updatePlatformConfigInstruction(admin, u))

  // creatorFeeRate and transferFeeExtensionAuth change only through AllInfo, which rewrites every
  // AllInfo field at once; otherwise each field that differs gets its own variant.
  if (c.creatorFeeRate !== spec.creatorFeeRate || !sameKey(c.transferFeeExtensionAuth, spec.transferFeeExtensionAuth)) {
    const { admin: _, ...info } = spec
    update({ allInfo: info })
  } else {
    if (!sameKey(c.platformFeeWallet, spec.feeWallet)) update({ feeWallet: spec.feeWallet })
    if (!sameKey(c.platformNftWallet, spec.nftWallet)) update({ nftWallet: spec.nftWallet })
    const m = spec.migrateNftInfo
    if (c.platformScale !== m.platformScale || c.creatorScale !== m.creatorScale || c.burnScale !== m.burnScale) update({ migrateNftInfo: m })
    if (c.feeRate !== spec.feeRate) update({ feeRate: spec.feeRate })
    if (c.name !== spec.name) update({ name: spec.name })
    if (c.web !== spec.web) update({ web: spec.web })
    if (c.img !== spec.img) update({ img: spec.img })
    if (!sameKey(c.cpswapConfig, spec.cpswapConfig)) update({ cpswapConfig: spec.cpswapConfig })
    if (!sameKey(c.platformVestingWallet, spec.vestingWallet)) update({ vestingWallet: spec.vestingWallet })
    if (c.platformVestingScale !== spec.platformVestingScale) update({ platformVestingScale: spec.platformVestingScale })
  }
  if (!sameKey(c.platformCpCreator, spec.platformCpCreator)) update({ platformCpCreator: spec.platformCpCreator })
  if (!sameKey(c.curveRuleManager, spec.curveRuleManager)) update({ curveRuleManager: spec.curveRuleManager })

  for (const g of live.allowed) if (!spec.globalConfigs.some((s) => s.address.equals(g))) out.push(closeAllowConfigInstruction(admin, g))
  for (const g of spec.globalConfigs) {
    if (!live.allowed.some((a) => a.equals(g.address))) out.push(createAllowConfigInstruction(admin, g.address))
    if (!g.ruleGroups) continue
    // Rule instructions follow any manager change above, so the spec's manager signs them.
    const manager = spec.curveRuleManager, rule = live.rules.get(g.address.toBase58()) ?? null
    if (!rule) out.push(createCurveRuleInstruction(manager, platform, g.address))
    const groups = rule?.groups ?? []
    for (const group of groups) if (group.groupId >= g.ruleGroups.length) out.push(removeCurveRuleGroupInstruction(manager, platform, g.address, group.groupId))
    g.ruleGroups.forEach((constraints, id) => {
      const current = groups.find((x) => x.groupId === id)
      if (!current || !sameGroup(current.constraints, constraints)) out.push(updateCurveRuleInstruction(manager, platform, g.address, id, constraints))
    })
  }

  // Restrictions go on last, once the allow configs and rules they enforce are in place.
  if (!!c.restrictGlobalConfig !== spec.restrictGlobalConfig) update({ restrictGlobalConfig: spec.restrictGlobalConfig })
  if (!!c.restrictCurveParam !== spec.restrictCurveParam) update({ restrictCurveParam: spec.restrictCurveParam })
  return out
}
