import { PublicKey, SystemProgram, type AccountInfo, type Connection, type TransactionInstruction } from '@solana/web3.js'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID, NATIVE_MINT, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountIdempotentInstruction, unpackAccount,
} from '@solana/spl-token'
import {
  CPMM_AUTHORITY, CPMM_PROGRAM_ID, CREATOR_FEE_VAULT_AUTHORITY, LAUNCHLAB_PROGRAM_ID,
  ata, creatorFeeShareAddress, creatorFeeVault, pda, poolAddress,
} from '../engines/launchpad/launchlab/addresses.js'
import { decodePoolState, type PoolState } from '../engines/launchpad/launchlab/accounts.js'
import { collectCreatorFeePermissionlessInstruction, creatorFeeShareRate, decodeAmmConfig, decodeCpmmPool } from '../engines/launchpad/launchlab/cpmm.js'
import { creatorFeePayout } from '../engines/launchpad/curve/cpmm.js'
import { claimCreatorFeeInstruction, unwrapAfterClaim } from '../engines/launchpad/launchlab/fees.js'
import { decodeTransaction } from '../engines/indexer/decoder.js'
import { connectionRpc, readTransaction } from '../engines/indexer/rpc.js'
import { descendants } from '../engines/indexer/token-flows.js'
import type { NormalizedInstruction } from '../engines/indexer/transaction.js'
import { readBuybackReceipt, receiptNativeClaimDelta, receiptTokenDelta } from './receipt.js'
import type { ClaimInput, ClaimPlan, ClaimPreview, ClaimedAmounts, LaunchpadAdapter } from './adapter.js'

const CLAIM = Buffer.from([26, 97, 138, 203, 132, 171, 141, 252])
const COLLECT = Buffer.from([202, 202, 34, 83, 226, 122, 145, 229])
const MIGRATE = Buffer.from([136, 92, 200, 103, 28, 218, 144, 140])
const tagged = (ix: NormalizedInstruction, program: PublicKey, tag: Buffer) => ix.programId === program.toBase58() && Buffer.from(ix.data).equals(tag)
const vaultFor = (pool: PublicKey, mint: PublicKey) => pda(CPMM_PROGRAM_ID, 'pool_vault', pool, mint)
const requireKey = (actual: string | undefined, expected: PublicKey) => {
  if (actual !== expected.toBase58()) throw new Error('LaunchLab claim receipt has an unexpected account.')
}

function tokenAccount(address: PublicKey, info: AccountInfo<Buffer> | null | undefined, mint: PublicKey, owner: PublicKey, program: PublicKey) {
  if (!info) return null
  const account = unpackAccount(address, info, program)
  if (!account.mint.equals(mint) || !account.owner.equals(owner)) throw new Error('LaunchLab fee account has an unexpected mint or authority.')
  return account
}

function validateLaunch(pool: PoolState, { mint, wallet, tokenProgram }: ClaimInput) {
  if (!pool.baseMint.equals(mint) || !pool.quoteMint.equals(NATIVE_MINT) || !pool.creator.equals(wallet)) throw new Error('LaunchLab pool does not belong to this mint and launch wallet.')
  const baseProgram = pool.tokenProgramFlag & 1 ? TOKEN_2022_PROGRAM_ID : TOKEN_PROGRAM_ID
  if (!baseProgram.equals(tokenProgram) || (pool.tokenProgramFlag & 2) !== 0 || ![0, 1, 2].includes(pool.status)) throw new Error('LaunchLab pool has unexpected token programs or status.')
  if (pool.status === 2 && pool.migrateType !== 1) throw new Error('LaunchLab pool did not graduate to CPMM.')
}

/** Read the address actually passed to a successful migration, including a non-PDA CPMM pool. */
export function launchlabMigratedPool(raw: unknown, pool: PoolState): PublicKey | null {
  const tx = decodeTransaction(raw, [])
  if (!tx.valid || tx.failed) return null
  for (const ix of tx.instructions) {
    if (!tagged(ix, LAUNCHLAB_PROGRAM_ID, MIGRATE) || ix.accounts.length < 28 || ix.accounts[17] !== pool.address.toBase58()) continue
    requireKey(ix.accounts[1], pool.baseMint)
    requireKey(ix.accounts[2], pool.quoteMint)
    requireKey(ix.accounts[3], pool.platformConfig)
    requireKey(ix.accounts[4], CPMM_PROGRAM_ID)
    requireKey(ix.accounts[18], pool.globalConfig)
    requireKey(ix.accounts[19], pool.baseVault)
    requireKey(ix.accounts[20], pool.quoteVault)
    return new PublicKey(ix.accounts[5])
  }
  return null
}

/** Creator fees only: curve fees and the migrated CPMM's booked creator-fee buckets. */
export function createLaunchlabAdapter(connection: Connection): LaunchpadAdapter {
  let migrated: PublicKey | null = null
  let historyBefore: string | undefined
  let candidates: string[] = []
  let historyEnded = false

  async function findMigration(pool: PoolState): Promise<PublicKey> {
    if (migrated) return migrated
    if (!candidates.length && !historyEnded) {
      const page = await connection.getSignaturesForAddress(pool.address, { limit: 100, ...(historyBefore ? { before: historyBefore } : {}) }, 'confirmed')
      candidates = page.filter(row => row.err === null).map(row => row.signature)
      historyBefore = page.at(-1)?.signature ?? historyBefore
      historyEnded = page.length < 100
    }
    // Discovery is a one-time scan. Keep a cursor and bound its RPC work per cycle.
    for (let read = 0; read < 4 && candidates.length; read++) {
      const raw = await readTransaction(connectionRpc(connection), candidates[0])
      if (!raw) throw new Error('LaunchLab migration history is not available yet.')
      const address = launchlabMigratedPool(raw, pool)
      candidates.shift()
      if (address) return (migrated = address)
    }
    if (!candidates.length && historyEnded) throw new Error('LaunchLab migration was not found in this RPC history.')
    throw new Error('LaunchLab migration discovery will continue on the next cycle.')
  }

  async function claimPlan(input: ClaimInput, preview = false): Promise<ClaimPlan & { preview: ClaimPreview }> {
    const { mint, wallet, tokenProgram } = input
    const launchAddress = poolAddress(mint, NATIVE_MINT)
    const curveVault = creatorFeeVault(wallet, NATIVE_MINT)
    const walletWsol = ata(wallet, NATIVE_MINT)
    const [launchInfo, curveInfo, walletWsolInfo] = await connection.getMultipleAccountsInfo([launchAddress, curveVault, walletWsol], 'confirmed')
    const launch = decodePoolState(launchAddress, launchInfo)
    validateLaunch(launch, input)
    const curveFees = tokenAccount(curveVault, curveInfo, NATIVE_MINT, CREATOR_FEE_VAULT_AUTHORITY, TOKEN_PROGRAM_ID)?.amount ?? 0n
    tokenAccount(walletWsol, walletWsolInfo, NATIVE_MINT, wallet, TOKEN_PROGRAM_ID)
    const instructions: TransactionInstruction[] = []
    let cpmmLamports = 0n
    let cpmmTokenAmount = 0n
    if (curveFees > 0n) instructions.push(claimCreatorFeeInstruction({ creator: wallet, quoteMint: NATIVE_MINT }))

    if (launch.status === 2) {
      const address = await findMigration(launch)
      const baseVault = vaultFor(address, mint)
      const quoteVault = vaultFor(address, NATIVE_MINT)
      const baseAta = ata(wallet, mint, tokenProgram)
      const [poolInfo, baseInfo, quoteInfo, recipientInfo] = await connection.getMultipleAccountsInfo([address, baseVault, quoteVault, baseAta], 'confirmed')
      const pool = decodeCpmmPool(address, poolInfo)
      if (!pool.poolCreator.equals(wallet)) throw new Error('The migrated CPMM creator is not LAUNCH_WALLET.')
      const baseIsZero = pool.mint0.equals(mint)
      if (!(baseIsZero ? pool.mint1.equals(NATIVE_MINT) : pool.mint0.equals(NATIVE_MINT) && pool.mint1.equals(mint))) throw new Error('Migrated CPMM has unexpected token mints.')
      if (!(baseIsZero ? pool.mint1 : pool.mint0).equals(NATIVE_MINT) || !(baseIsZero ? pool.tokenProgram0 : pool.tokenProgram1).equals(tokenProgram) || !(baseIsZero ? pool.tokenProgram1 : pool.tokenProgram0).equals(TOKEN_PROGRAM_ID)) throw new Error('Migrated CPMM has unexpected mints or token programs.')
      if (!(baseIsZero ? pool.vault0 : pool.vault1).equals(baseVault) || !(baseIsZero ? pool.vault1 : pool.vault0).equals(quoteVault)) throw new Error('Migrated CPMM has unexpected token vaults.')
      if (!tokenAccount(baseVault, baseInfo, mint, CPMM_AUTHORITY, tokenProgram) || !tokenAccount(quoteVault, quoteInfo, NATIVE_MINT, CPMM_AUTHORITY, TOKEN_PROGRAM_ID)) throw new Error('Migrated CPMM token vault is missing.')
      tokenAccount(baseAta, recipientInfo, mint, wallet, tokenProgram)
      if (pool.creatorFees0 > 0n || pool.creatorFees1 > 0n) {
        instructions.push(
          createAssociatedTokenAccountIdempotentInstruction(wallet, walletWsol, wallet, NATIVE_MINT),
          createAssociatedTokenAccountIdempotentInstruction(wallet, baseAta, wallet, mint, tokenProgram),
          collectCreatorFeePermissionlessInstruction(wallet, pool),
        )
        if (preview) {
          const [configInfo, shareInfo] = await connection.getMultipleAccountsInfo([pool.ammConfig, creatorFeeShareAddress(wallet, pool.ammConfig)], 'confirmed')
          const share = creatorFeeShareRate(decodeAmmConfig(configInfo), shareInfo, wallet, pool.ammConfig)
          cpmmLamports = creatorFeePayout(baseIsZero ? pool.creatorFees1 : pool.creatorFees0, share).creator
          cpmmTokenAmount = creatorFeePayout(baseIsZero ? pool.creatorFees0 : pool.creatorFees1, share).creator
        }
      }
    }
    if (instructions.length) instructions.push(...unwrapAfterClaim(wallet, NATIVE_MINT))
    return { instructions, preview: { curveLamports: curveFees, cpmmLamports, cpmmTokenAmount } }
  }
  return {
    claimPlan: input => claimPlan(input),
    async previewAmounts(input) {
      const plan = await claimPlan(input, true)
      return plan.preview
    },
    claimedAmounts: launchlabClaimedAmounts,
  }
}

/** Measure wallet income and reconcile its curve/CPMM split to this claim's own token transfers. */
export function launchlabClaimedAmounts(raw: unknown, input: ClaimInput): ClaimedAmounts {
  const { mint, wallet, tokenProgram } = input
  const tx = decodeTransaction(raw, [])
  const receipt = readBuybackReceipt(raw, tx.signature, wallet.toBase58())
  if (tx.issues.length) throw new Error('Claim receipt contains incomplete instruction or token-balance data.')
  const wsolAta = ata(wallet, NATIVE_MINT)
  const baseAta = ata(wallet, mint, tokenProgram)
  let curveLamports = 0n
  let cpmmLamports = 0n
  let grossTokens = 0n
  let claims = 0
  for (const ix of tx.instructions.filter(ix => ix.parentIndex === null)) {
    const expected: { source: PublicKey; destination: PublicKey; mint: PublicKey; authority: PublicKey; program: PublicKey }[] = []
    if (tagged(ix, LAUNCHLAB_PROGRAM_ID, CLAIM)) {
      if (ix.accounts.length !== 8) throw new Error('LaunchLab creator claim has an unexpected account list.')
      const accounts = [wallet, CREATOR_FEE_VAULT_AUTHORITY, creatorFeeVault(wallet, NATIVE_MINT), wsolAta, NATIVE_MINT, TOKEN_PROGRAM_ID, SystemProgram.programId, ASSOCIATED_TOKEN_PROGRAM_ID]
      accounts.forEach((account, index) => requireKey(ix.accounts[index], account))
      expected.push({ source: accounts[2], destination: wsolAta, mint: NATIVE_MINT, authority: CREATOR_FEE_VAULT_AUTHORITY, program: TOKEN_PROGRAM_ID })
    } else if (tagged(ix, CPMM_PROGRAM_ID, COLLECT)) {
      if (ix.accounts.length !== 16) throw new Error('CPMM creator collect has an unexpected account list.')
      requireKey(ix.accounts[0], wallet)
      requireKey(ix.accounts[1], wallet)
      requireKey(ix.accounts[2], CPMM_AUTHORITY)
      const pool = new PublicKey(ix.accounts[3])
      const zeroIsBase = ix.accounts[6] === mint.toBase58()
      const mints = zeroIsBase ? [mint, NATIVE_MINT] : [NATIVE_MINT, mint]
      const programs = zeroIsBase ? [tokenProgram, TOKEN_PROGRAM_ID] : [TOKEN_PROGRAM_ID, tokenProgram]
      for (let side = 0; side < 2; side++) {
        const source = vaultFor(pool, mints[side])
        const destination = ata(wallet, mints[side], programs[side])
        requireKey(ix.accounts[4 + side], source)
        requireKey(ix.accounts[6 + side], mints[side])
        requireKey(ix.accounts[8 + side], destination)
        requireKey(ix.accounts[10 + side], programs[side])
        expected.push({ source, destination, mint: mints[side], authority: CPMM_AUTHORITY, program: programs[side] })
      }
      requireKey(ix.accounts[12], ASSOCIATED_TOKEN_PROGRAM_ID)
      requireKey(ix.accounts[13], SystemProgram.programId)
      requireKey(ix.accounts[15], creatorFeeShareAddress(wallet, new PublicKey(ix.accounts[14])))
    } else continue
    claims++
    const flows = descendants(tx, ix)
    if (!flows.length) throw new Error('Claim receipt is missing its token transfer instructions.')
    for (const flow of flows) {
      const match = expected.find(account => flow.kind === 'transfer' && flow.source === account.source.toBase58() && flow.destination === account.destination.toBase58() && flow.authority === account.authority.toBase58())
      const tokenIx = tx.instructions.find(instruction => instruction.path.index === flow.path.index)
      if (!match || flow.mint !== match.mint.toBase58() || tokenIx?.programId !== match.program.toBase58()) throw new Error('Claim transferred an unexpected token or account.')
      const sourceBalances = tx.tokenBalances.filter(balance => balance.account === flow.source)
      if (!sourceBalances.length || sourceBalances.some(balance => balance.mint !== match.mint.toBase58() || balance.owner !== match.authority.toBase58())) throw new Error('Claim source balance has an unexpected mint or owner.')
      const destinationBalances = tx.tokenBalances.filter(balance => balance.account === flow.destination)
      if (destinationBalances.some(balance => balance.mint !== match.mint.toBase58() || balance.owner !== wallet.toBase58())) throw new Error('Claim destination has an unexpected mint or owner.')
      if (match.mint.equals(NATIVE_MINT)) {
        if (ix.programId === LAUNCHLAB_PROGRAM_ID.toBase58()) curveLamports += BigInt(flow.amountRaw)
        else cpmmLamports += BigInt(flow.amountRaw)
      }
      else grossTokens += BigInt(flow.amountRaw)
    }
    for (const account of expected) {
      const balances = tx.tokenBalances.filter(balance => balance.account === account.source.toBase58())
      const pre = balances.filter(balance => balance.side === 'pre')
      const post = balances.filter(balance => balance.side === 'post')
      const transferred = flows.filter(flow => flow.source === account.source.toBase58()).reduce((sum, flow) => sum + BigInt(flow.amountRaw), 0n)
      if (pre.length !== 1 || post.length !== 1 || BigInt(pre[0].raw) - BigInt(post[0].raw) !== transferred) throw new Error('Claim source balance delta differs from its transfers.')
    }
  }
  if (!claims) throw new Error('Receipt contains no LaunchLab or CPMM creator-fee claim.')
  const closed = tx.instructions.some(ix => ix.parentIndex === null && ix.programId === TOKEN_PROGRAM_ID.toBase58() && ix.data.length === 1 && ix.data[0] === 9 && ix.accounts.length === 3 && ix.accounts[0] === wsolAta.toBase58() && ix.accounts[1] === wallet.toBase58() && ix.accounts[2] === wallet.toBase58())
  if (!closed) throw new Error('Claim did not unwrap the launch wallet WSOL account.')
  const touchedBase = tx.tokenBalances.some(balance => balance.account === baseAta.toBase58())
  const tokenAmount = touchedBase ? receiptTokenDelta(receipt, input) : 0n
  if (tokenAmount < 0n || tokenAmount > grossTokens || (grossTokens > 0n && !touchedBase)) throw new Error('Claim token balance delta differs from its transfers.')
  const lamports = receiptNativeClaimDelta(receipt, input)
  if (lamports !== curveLamports + cpmmLamports) throw new Error('Claim wallet income differs from its creator-fee transfers.')
  return { lamports, tokenAmount, curveLamports, cpmmLamports, cpmmTokenAmount: tokenAmount }
}
