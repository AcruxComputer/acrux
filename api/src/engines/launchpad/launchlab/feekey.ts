// The Fee Key: the LP-Lock position a migration locks the platform's LP share into, minted as an
// NFT to the platform's nft wallet. Its record decoded, a graduated pool's Fee Keys found, and
// the harvest of its fee LP (paid out in both pool tokens) built.

import { PublicKey, TransactionInstruction, type AccountInfo, type Connection } from '@solana/web3.js'
import { TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, createAssociatedTokenAccountIdempotentInstruction } from '@solana/spl-token'
import { CPMM_AUTHORITY, CPMM_PROGRAM_ID, LOCK_AUTHORITY, LP_LOCK_PROGRAM_ID, MEMO_PROGRAM_ID, VAULT_AUTHORITY, ata, lockedLiquidityAddress } from './addresses.js'
import type { CpmmPool } from './cpmm.js'
import type { LockedLp } from '../curve/cpmm.js'

const LOCKED_LIQUIDITY = Buffer.from([25, 10, 238, 197, 207, 234, 73, 22])
const COLLECT_CP_FEES = Buffer.from([8, 30, 51, 199, 209, 184, 247, 133])
const U64_MAX = (1n << 64n) - 1n

export type FeeKey = LockedLp & {
  address: PublicKey; claimedLpAmount: bigint; pool: PublicKey; feeNftMint: PublicKey
  /** Who locked it. A migration's lock names LaunchLab's vault authority; anything else is someone else's lock. */
  lockedOwner: PublicKey
  lpMint: PublicKey
}

export function decodeFeeKey(address: PublicKey, info: Pick<AccountInfo<Buffer>, 'owner' | 'data'> | null | undefined): FeeKey {
  const d = info?.data
  if (!info || !d || !info.owner.equals(LP_LOCK_PROGRAM_ID) || d.length < 192 || !d.subarray(0, 8).equals(LOCKED_LIQUIDITY)) throw new Error('not an LP-Lock CPMM position')
  const key = (o: number) => new PublicKey(d.subarray(o, o + 32))
  return {
    address, lockedLpAmount: d.readBigUInt64LE(8), claimedLpAmount: d.readBigUInt64LE(16), unclaimedLpAmount: d.readBigUInt64LE(24),
    lastLp: d.readBigUInt64LE(32), lastK: d.readBigUInt64LE(40) + (d.readBigUInt64LE(48) << 64n),
    pool: key(64), feeNftMint: key(96), lockedOwner: key(128), lpMint: key(160),
  }
}

/** True for the Fee Key a migration made: locked by LaunchLab, on this pool and its LP mint. */
export const isMigrationFeeKey = (k: FeeKey, pool: Pick<CpmmPool, 'address' | 'lpMint'>) =>
  k.lockedOwner.equals(VAULT_AUTHORITY) && k.pool.equals(pool.address) && k.lpMint.equals(pool.lpMint)

/**
 * The Fee Keys LaunchLab locked on a pool. Anyone can lock dust LP of a graduated pool and name any
 * wallet as its NFT owner, so this filters on locked_owner = LaunchLab's vault authority.
 */
export async function findFeeKeys(connection: Connection, pool: PublicKey): Promise<FeeKey[]> {
  const rows = await connection.getProgramAccounts(LP_LOCK_PROGRAM_ID, { commitment: 'confirmed', filters: [
    { dataSize: 256 },
    { memcmp: { offset: 64, bytes: pool.toBase58() } },
    { memcmp: { offset: 128, bytes: VAULT_AUTHORITY.toBase58() } },
  ] })
  return rows.map((r) => decodeFeeKey(r.pubkey, r.account))
}

export const readFeeKey = async (connection: Connection, feeNftMint: PublicKey) => {
  const address = lockedLiquidityAddress(feeNftMint)
  return decodeFeeKey(address, await connection.getAccountInfo(address, 'confirmed'))
}

/**
 * Harvest a Fee Key: LP-Lock withdraws the accrued fee LP from CPMM and pays both tokens to the
 * NFT holder's ATAs (created first, idempotently). LP-Lock pays the smaller of what is asked and
 * what has accrued, so the default asks for u64::MAX. Buys pay the LP fee in the quote token,
 * sells in the coin. A harvest whose fee LP floors one side to zero, or a pool with withdraw
 * off, fails inside CPMM: anything that must always succeed runs with the harvest optional.
 */
export function collectFeeKeyInstructions(input: {
  owner: PublicKey; payer?: PublicKey; pool: CpmmPool; feeNftMint: PublicKey; lpAmount?: bigint
  /** Token accounts that receive token 0 and token 1; the owner's ATAs (opened first) when unset. */
  recipients?: [PublicKey, PublicKey]
}): TransactionInstruction[] {
  const { owner, pool } = input
  const payer = input.payer ?? owner
  const [r0, r1] = input.recipients ?? [ata(owner, pool.mint0, pool.tokenProgram0), ata(owner, pool.mint1, pool.tokenProgram1)]
  const open = input.recipients ? [] : [
    createAssociatedTokenAccountIdempotentInstruction(payer, r0, owner, pool.mint0, pool.tokenProgram0),
    createAssociatedTokenAccountIdempotentInstruction(payer, r1, owner, pool.mint1, pool.tokenProgram1),
  ]
  const w = (pubkey: PublicKey, isSigner = false) => ({ pubkey, isSigner, isWritable: true })
  const r = (pubkey: PublicKey, isSigner = false) => ({ pubkey, isSigner, isWritable: false })
  const body = Buffer.alloc(16)
  COLLECT_CP_FEES.copy(body)
  body.writeBigUInt64LE(input.lpAmount ?? U64_MAX, 8)
  return [
    ...open,
    new TransactionInstruction({
      programId: LP_LOCK_PROGRAM_ID, data: body,
      keys: [
        r(LOCK_AUTHORITY), r(owner, true), r(ata(owner, input.feeNftMint, TOKEN_PROGRAM_ID)), w(lockedLiquidityAddress(input.feeNftMint)),
        r(CPMM_PROGRAM_ID), r(CPMM_AUTHORITY), w(pool.address), w(pool.lpMint), w(r0), w(r1), w(pool.vault0), w(pool.vault1),
        r(pool.mint0), r(pool.mint1), w(ata(LOCK_AUTHORITY, pool.lpMint)), r(TOKEN_PROGRAM_ID), r(TOKEN_2022_PROGRAM_ID), r(MEMO_PROGRAM_ID),
      ],
    }),
  ]
}
