// Every fee a LaunchLab coin has waiting, read at once: the curve's platform and creator fee
// vaults, the graduated CPMM pool's creator fee net of Raydium's share, and what its Fee Key can
// harvest now. The pool, its vaults and its fee counters come from one RPC read, so they agree.

import { type AccountInfo, type Commitment, type Connection, type PublicKey } from '@solana/web3.js'
import { creatorFeeShareAddress, creatorFeeVault, platformFeeVault, lockedLiquidityAddress } from './addresses.js'
import { creatorFeeShareRate, decodeAmmConfig, decodeCpmmPool, type CpmmPool } from './cpmm.js'
import { decodeFeeKey, isMigrationFeeKey } from './feekey.js'
import { cpmmReserves, creatorFeePayout, feeKeyAccrued, lpToTokens } from '../curve/cpmm.js'

export type FeeSnapshotInput = {
  platformConfig: PublicKey
  /** The launch creator, whose curve fees accrue in its creator vault. */
  creator: PublicKey
  quoteMint: PublicKey
  /** The graduated pool, as read from its migration. */
  cpmmPool?: PublicKey
  feeNftMint?: PublicKey
}

export type FeeSnapshot = {
  slot: number
  /** Quote waiting in the curve fee vaults, claimable with claim_platform_fee_from_vault / claim_creator_fee. */
  platformVault: bigint
  creatorVault: bigint
  cpmm: null | {
    pool: CpmmPool
    reserve0: bigint; reserve1: bigint
    /** Booked creator fees and what a collect pays the pool creator after Raydium's share. */
    creatorFees0: bigint; creatorFees1: bigint; creatorPayout0: bigint; creatorPayout1: bigint; shareRate: bigint
    /** Fee LP the Fee Key can harvest and the tokens it withdraws; all zero when a side would floor to zero. */
    feeKey: null | { lp: bigint; amount0: bigint; amount1: bigint }
  }
}

const tokenAmount = (info: AccountInfo<Buffer> | null | undefined) => (info && info.data.length >= 72 ? info.data.readBigUInt64LE(64) : 0n)

export async function readFeeSnapshot(connection: Connection, input: FeeSnapshotInput, commitment: Commitment = 'confirmed'): Promise<FeeSnapshot> {
  const vaults = [platformFeeVault(input.platformConfig, input.quoteMint), creatorFeeVault(input.creator, input.quoteMint)]
  if (!input.cpmmPool) {
    const { context, value } = await connection.getMultipleAccountsInfoAndContext(vaults, commitment)
    return { slot: context.slot, platformVault: tokenAmount(value[0]), creatorVault: tokenAmount(value[1]), cpmm: null }
  }
  // The pool's own address fixes its vaults' addresses only after it is decoded, so read it first,
  // then read it again together with everything that has to agree with it.
  const first = decodeCpmmPool(input.cpmmPool, await connection.getAccountInfo(input.cpmmPool, commitment))
  const lock = input.feeNftMint ? [lockedLiquidityAddress(input.feeNftMint)] : []
  const keys = [...vaults, input.cpmmPool, first.vault0, first.vault1, first.ammConfig, creatorFeeShareAddress(first.poolCreator, first.ammConfig), ...lock]
  const { context, value } = await connection.getMultipleAccountsInfoAndContext(keys, commitment)
  const pool = decodeCpmmPool(input.cpmmPool, value[2])
  const config = decodeAmmConfig(value[5])
  const shareRate = creatorFeeShareRate(config, value[6], pool.poolCreator, pool.ammConfig)
  const { reserve0, reserve1 } = cpmmReserves(tokenAmount(value[3]), tokenAmount(value[4]), pool)
  let feeKey: { lp: bigint; amount0: bigint; amount1: bigint } | null = null
  if (input.feeNftMint && value[7]) {
    const k = decodeFeeKey(lock[0]!, value[7])
    const withdrawOff = (pool.status & 2) !== 0
    if (isMigrationFeeKey(k, pool) && !withdrawOff) {
      const lp = feeKeyAccrued(k, pool.lpSupply, reserve0, reserve1), t = lpToTokens(lp, pool.lpSupply, reserve0, reserve1)
      feeKey = t.amount0 > 0n && t.amount1 > 0n ? { lp, ...t } : { lp: 0n, amount0: 0n, amount1: 0n }
    }
  }
  const pay0 = creatorFeePayout(pool.creatorFees0, shareRate), pay1 = creatorFeePayout(pool.creatorFees1, shareRate)
  return {
    slot: context.slot, platformVault: tokenAmount(value[0]), creatorVault: tokenAmount(value[1]),
    cpmm: {
      pool, reserve0, reserve1, creatorFees0: pool.creatorFees0, creatorFees1: pool.creatorFees1,
      creatorPayout0: pay0.creator, creatorPayout1: pay1.creator, shareRate, feeKey,
    },
  }
}
