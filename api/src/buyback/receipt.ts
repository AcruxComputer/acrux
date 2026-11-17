import { PublicKey, SystemProgram } from '@solana/web3.js'
import { ASSOCIATED_TOKEN_PROGRAM_ID, NATIVE_MINT, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token'
import { normalizeTransaction, type NormalizedTransaction } from '../engines/indexer/transaction.js'
import type { ClaimInput } from './adapter.js'

export type BuybackReceipt = { raw: unknown; transaction: NormalizedTransaction; preLamports: bigint[]; postLamports: bigint[] }

export function readBuybackReceipt(raw: unknown, signature: string, wallet: string): BuybackReceipt {
  const transaction = normalizeTransaction(raw)
  if (!transaction.valid || transaction.failed || transaction.signature !== signature || transaction.signers[0] !== wallet) throw new Error('Invalid buyback transaction receipt.')
  const balances = (side: 'preBalances' | 'postBalances') => {
    const values = (raw as { meta?: Record<string, unknown> }).meta?.[side]
    if (!Array.isArray(values) || values.length !== transaction.keys.length || values.some(value => typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0)) throw new Error('Receipt is missing exact transaction account balances.')
    return values.map(value => BigInt(value as number))
  }
  return { raw, transaction, preLamports: balances('preBalances'), postLamports: balances('postBalances') }
}

/** Wallet income after unwrapping, excluding fees, ATA rent and WSOL already owned before the claim. */
export function receiptNativeClaimDelta(receipt: BuybackReceipt, { wallet }: ClaimInput): bigint {
  const tx = receipt.transaction
  const walletIndex = tx.keys.findIndex(key => key.address === wallet.toBase58())
  if (walletIndex !== 0 || tx.feeRaw === null) throw new Error('Claim receipt is missing its wallet fee or balances.')
  let amount = receipt.postLamports[walletIndex] - receipt.preLamports[walletIndex] + BigInt(tx.feeRaw)
  const funded = new Set<string>()
  for (const ix of tx.instructions) {
    if (ix.programId !== ASSOCIATED_TOKEN_PROGRAM_ID.toBase58() || ix.accounts[0] !== wallet.toBase58()) continue
    // Create accepts legacy empty data or enum tag 0; CreateIdempotent uses tag 1.
    if (ix.accounts.length < 6 || !(ix.data.length === 0 || (ix.data.length === 1 && (ix.data[0] === 0 || ix.data[0] === 1)))) throw new Error('Claim contains an unsupported ATA instruction.')
    const [, account, owner, accountMint, system, program] = ix.accounts
    if (system !== SystemProgram.programId.toBase58() || ![TOKEN_PROGRAM_ID.toBase58(), TOKEN_2022_PROGRAM_ID.toBase58()].includes(program) || account !== getAssociatedTokenAddressSync(new PublicKey(accountMint), new PublicKey(owner), true, new PublicKey(program)).toBase58()) throw new Error('Claim contains a noncanonical ATA create.')
    if (funded.has(account)) continue
    funded.add(account)
    const index = tx.keys.findIndex(key => key.address === account)
    if (index < 0) throw new Error('Claim ATA is missing from transaction accounts.')
    if (receipt.preLamports[index] !== 0n || receipt.postLamports[index] === 0n) continue
    const post = tx.tokenBalances.filter(balance => balance.account === account && balance.side === 'post')
    if (post.length !== 1 || post[0].mint !== accountMint || post[0].owner !== owner || (accountMint === NATIVE_MINT.toBase58() && post[0].raw !== '0')) throw new Error('Claim retained ATA has unexpected token balances.')
    // A wallet-funded, newly opened ATA that remains holds its rent. New accounts closed
    // within this transaction refund their rent in the same wallet delta and add nothing.
    amount += receipt.postLamports[index]
  }
  const wsol = getAssociatedTokenAddressSync(NATIVE_MINT, wallet).toBase58()
  const wsolIndex = tx.keys.findIndex(key => key.address === wsol)
  if (wsolIndex >= 0 && receipt.preLamports[wsolIndex] > 0n && receipt.postLamports[wsolIndex] === 0n) {
    const closed = tx.instructions.some(ix => ix.programId === TOKEN_PROGRAM_ID.toBase58() && ix.data.length === 1 && ix.data[0] === 9 && ix.accounts.length === 3 && ix.accounts[0] === wsol && ix.accounts[1] === wallet.toBase58() && ix.accounts[2] === wallet.toBase58())
    const pre = tx.tokenBalances.filter(balance => balance.account === wsol && balance.side === 'pre')
    if (!closed || pre.length !== 1 || pre[0].mint !== NATIVE_MINT.toBase58() || pre[0].owner !== wallet.toBase58() || BigInt(pre[0].raw) > receipt.preLamports[wsolIndex]) throw new Error('Claim closed an invalid preexisting wallet WSOL account.')
    // Exclude both old WSOL principal and its rent refund from this claim's income.
    amount -= receipt.preLamports[wsolIndex]
  }
  if (amount < 0n) throw new Error('Claim receipt has negative normalized wallet income.')
  return amount
}

/** Delta of this wallet's ATA, never the account's full balance. */
export function receiptTokenDelta(receipt: BuybackReceipt, { mint, wallet, tokenProgram }: ClaimInput): bigint {
  const tx = receipt.transaction
  const account = getAssociatedTokenAddressSync(mint, wallet, false, tokenProgram).toBase58()
  const balances = tx.tokenBalances.filter(row => row.account === account)
  if (balances.some(row => row.mint !== mint.toBase58() || row.owner !== wallet.toBase58())) throw new Error('Receipt has an unexpected token owner or mint.')
  const pre = balances.filter(row => row.side === 'pre')
  const post = balances.filter(row => row.side === 'post')
  if (pre.length > 1 || post.length !== 1) throw new Error('Receipt is missing exact token balances.')
  // A missing pre-token row means zero only when this transaction created the ATA.
  const index = tx.keys.findIndex(key => key.address === account)
  if (index < 0 || (!pre.length && receipt.preLamports[index] !== 0n)) throw new Error('Receipt omitted an existing token account balance.')
  return BigInt(post[0].raw) - BigInt(pre[0]?.raw ?? '0')
}
