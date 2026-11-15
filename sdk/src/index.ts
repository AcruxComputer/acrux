import { ed25519 } from '@noble/curves/ed25519.js'
import { createGatewayClient, fromBase58, toBase64, type Slot, type Transaction, type Window } from '@acrux/contract'
import { invalid, unavailable } from './errors.js'
import { seal } from './threshold.js'
import { deriveHeader, headerBytes, transactionBytes, type SignedTransaction } from './transaction.js'

export { AcruxError, type ErrorCode } from './errors.js'
export { seal, open } from './threshold.js'
export { deriveHeader, headerBytes, type SignedTransaction } from './transaction.js'
export type { FragmentSubmission, Slot, Transaction, TxHeader } from '@acrux/contract'

/** The fee payer's signer: a `Keypair`, or a function such as a wallet's `signMessage`. */
export type Signer = { secretKey: Uint8Array } | ((message: Uint8Array) => Uint8Array | Promise<Uint8Array>)

/** n proposers receive the transaction; it becomes readable once t of them include it. t = 1 sends it in the clear. */
export type SendOptions = { n?: number; t?: number; signer?: Signer }

export type TxUpdate = {
  id: string
  state: 'sent' | 'included' | 'executed' | 'finalized' | 'failed' | 'expired'
  /** Fragments included so far, of the `threshold` that make the transaction readable. */
  included: number
  threshold: number
  transaction: Transaction
}

/** Slots between the latest started slot and the one a send targets. */
const TARGET_LEAD = 4
const STATES = { sent: 0, partly_included: 1, executed: 2, finalized: 3, failed: 3, expired: 3 } as const

export function connect(rpcUrl: string, options: { wsUrl?: string } = {}) {
  const gateway = createGatewayClient({ rpcUrl, wsUrl: options.wsUrl ?? streamUrl(rpcUrl) })

  async function receivers(targetSlot: number, n: number): Promise<number[]> {
    const holds = (window: Window) => window.firstSlot <= targetSlot && targetSlot <= window.lastSlot
    const current = await gateway.call('getCurrentWindow', {})
    const window = holds(current) ? current : (await gateway.call('getSchedule', { limit: 8 })).find(holds)
    if (!window) return [1, 2, 3].slice(0, n)
    const online = window.proposers.filter(p => p.status === 'online')
    return [...online, ...window.proposers.filter(p => !online.includes(p))].slice(0, n).map(p => p.index)
  }

  return {
    /** The contract's gateway client, for every read the gateway serves. */
    gateway,

    /** The blockhash a transaction for this net is signed over. */
    latestBlockhash: () => gateway.call('getLatestBlockhash', {}),

    async send(transaction: SignedTransaction, { n = 1, t = 1, signer }: SendOptions = {}) {
      if (![1, 2, 3].includes(n) || ![1, 2, 3].includes(t) || t > n) throw invalid('Require 1 <= t <= n <= 3.')
      if (t > 1 && !signer) throw invalid('A threshold send needs the fee payer as `signer`.')
      const bytes = transactionBytes(transaction)
      const latest = (await gateway.call('getRecentSlots', { limit: 1 })).at(-1)
      const header = deriveHeader(bytes, (latest?.slot ?? 0) + TARGET_LEAD)
      const to = await receivers(header.targetSlot, n)

      if (t === 1) await gateway.call('submitTransaction', { header, transaction: toBase64(bytes), receivers: to })
      else {
        const message = headerBytes(header)
        const signature = typeof signer === 'function' ? await signer(message) : ed25519.sign(message, signer!.secretKey.subarray(0, 32))
        if (!ed25519.verify(signature, message, fromBase58(header.feePayer))) throw invalid('`signer` is not the transaction\'s fee payer.')
        await Promise.all(seal(bytes, header, signature, t, to).map(fragment => gateway.call('submitFragment', fragment)))
      }
      return { id: header.id, targetSlot: header.targetSlot }
    },

    tx: {
      /** The transaction's trail as it happens; ends after `finalized`, `failed` or `expired`. */
      async *follow(id: string): AsyncGenerator<TxUpdate, void, void> {
        const queue: (Transaction | Error)[] = []
        let wake = () => {}
        const push = (item: Transaction | Error) => { queue.push(item); wake() }
        const stream = gateway.subscribe({ kind: 'transaction', id }, event => {
          if (event.type === 'transaction.updated' && event.data.header.id === id) push(event.data)
        }, {
          // Whatever happened before the stream opened is in the gateway's record.
          onOpen: () => { gateway.call('getTransaction', { id }).then(push, () => {}) },
          onClose: () => push(unavailable('The event stream closed.')),
        })
        try {
          for (let reached = -1; ;) {
            while (!queue.length) await new Promise<void>(resolve => { wake = resolve })
            const transaction = queue.shift()!
            if (transaction instanceof Error) throw transaction
            if (transaction.state === 'unknown') continue
            const stage = STATES[transaction.state]
            const progress = stage * 4 + transaction.includedCount
            if (progress <= reached) continue
            reached = progress
            yield {
              id,
              state: transaction.state === 'partly_included' ? 'included' : transaction.state,
              included: transaction.includedCount,
              threshold: transaction.t,
              transaction,
            }
            if (stage === 3) return
          }
        } finally {
          stream.close()
        }
      },
    },

    slots: {
      /** Calls back with each slot as it starts and again as it executes. Returns the function that stops it. */
      subscribe(onSlot: (slot: Slot) => void): () => void {
        // Every validator reports the same slot; the first report of each stage is the one delivered.
        const latest = { 'slot.started': -1, 'slot.executed': -1 }
        const stream = gateway.subscribe({ kind: 'slots', from: 0, to: Number.MAX_SAFE_INTEGER }, event => {
          if ((event.type !== 'slot.started' && event.type !== 'slot.executed') || event.data.slot <= latest[event.type]) return
          latest[event.type] = event.data.slot
          onSlot(event.data)
        })
        return stream.close
      },
    },
  }
}

export type Acrux = ReturnType<typeof connect>

/** The event stream listens one port above JSON-RPC. */
function streamUrl(rpcUrl: string): string {
  const url = new URL(rpcUrl)
  const secure = url.protocol === 'https:'
  url.port = String(Number(url.port || (secure ? 443 : 80)) + 1)
  url.protocol = secure ? 'wss:' : 'ws:'
  return url.href
}
