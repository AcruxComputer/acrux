import { PublicKey, SystemInstruction, SystemProgram, type TransactionInstruction } from '@solana/web3.js'
import { NATIVE_MINT, TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync } from '@solana/spl-token'
import { createJupiterClient } from '../engines/jupiter/http.js'
import { quoteExactIn } from '../engines/jupiter/quote.js'
import { swapInstructions } from '../engines/jupiter/swap.js'

export type SwapPlan = { instructions: TransactionInstruction[]; lookupTables: PublicKey[]; minimumOut: bigint }
export type Swapper = {
  instructions(input: { mint: PublicKey; wallet: PublicKey; tokenProgram: PublicKey; lamports: bigint; slippageBps: number }): Promise<SwapPlan>
}

export function createBuybackSwapper(apiKey?: string): Swapper {
  const client = createJupiterClient({ apiKey })
  return {
    async instructions({ mint, wallet, tokenProgram, lamports, slippageBps }) {
      const destination = getAssociatedTokenAddressSync(mint, wallet, false, tokenProgram)
      const quote = await quoteExactIn(client, { inputMint: NATIVE_MINT, outputMint: mint, amount: lamports, slippageBps, maxAccounts: 40 })
      const route = await swapInstructions(client, { quote, taker: wallet, destination })
      const wsol = getAssociatedTokenAddressSync(NATIVE_MINT, wallet)
      let wrapped = 0n
      for (const ix of route.setup) {
        if (ix.programId.equals(SystemProgram.programId)) {
          let transfer: ReturnType<typeof SystemInstruction.decodeTransfer>
          try { transfer = SystemInstruction.decodeTransfer(ix) } catch { throw new Error('Jupiter returned an unsupported SOL instruction.') }
          if (ix.keys.length !== 2 || !transfer.fromPubkey.equals(wallet) || !transfer.toPubkey.equals(wsol)) throw new Error('Jupiter returned an unexpected SOL transfer.')
          wrapped += BigInt(transfer.lamports)
        } else if (ix.programId.equals(TOKEN_PROGRAM_ID) && (ix.keys.length !== 1 || ix.data.length !== 1 || ix.data[0] !== 17 || !ix.keys[0].pubkey.equals(wsol))) {
          throw new Error('Jupiter returned an unexpected token instruction.')
        }
      }
      if (wrapped !== lamports) throw new Error('Jupiter SOL wrap differs from the requested swap amount.')
      return { instructions: [...route.setup, route.swap, ...route.cleanup], lookupTables: route.lookupTables, minimumOut: quote.minimumOut }
    },
  }
}
