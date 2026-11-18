import { ACRUX_BRIDGE_IDL } from '@acrux/contract'
import { BorshAccountsCoder, Program, type Idl } from '@coral-xyz/anchor'
import BN from 'bn.js'
import type { Prisma, PrismaClient } from '@prisma/client'
import { Connection, PublicKey, SystemProgram, SYSVAR_INSTRUCTIONS_PUBKEY, type AccountInfo, type Keypair, type TransactionInstruction } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, createAssociatedTokenAccountIdempotentInstruction, getAssociatedTokenAddressSync, unpackMint } from '@solana/spl-token'
import { createJournal, type KeeperTxRow } from '../engines/keeper/journal.js'
import { prismaKeeperStore } from '../engines/keeper/prisma-store.js'
import type { BridgeRelayerConfig } from './config.js'
import { attestInstruction, exact, outboxProof, readProof, u64, type Entry, type Proof } from './proof.js'
import { attestationBatches, pendingFinalityStore } from './transactions.js'

/** An error's message for the log, with any URL (which may carry an RPC key) left out. */
export const reason = (error: unknown) => error instanceof Error ? `${error.name}: ${error.message.replace(/https?:\/\/\S+/g, '<url>')}` : 'Error'
const RPC_MS = 10_000
const CONFIRM_MS = 30_000
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))

/** HTTP-only connections: every request is aborted; confirmation uses the keeper journal's bounded poll. */
export function bridgeConnection(url: string) {
  return new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true, fetch: (input, init) => fetch(input, {
    ...init, signal: init?.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(RPC_MS)]) : AbortSignal.timeout(RPC_MS),
  }) })
}

type Finality = 'confirmed' | 'finalized'

type ValidatorSet = { epoch: BN; validators: { key: PublicKey; stake: BN }[]; payer: PublicKey }
type Cblock = { slot: BN; epoch: BN; hash: number[]; certified: boolean; notar: number; finalize: number; payer: PublicKey }
type Withdrawal = { sequence: BN; recipient: PublicKey; solana_mint: PublicKey; amount: BN; slot: BN }

export function createBridgeRelayer(options: { config: BridgeRelayerConfig; signer: Keypair; db: PrismaClient; isLeader: () => boolean; log: (entry: Record<string, unknown>) => void }) {
  const { config, signer, db, isLeader, log } = options
  const solana = bridgeConnection(config.rpcUrl), acrux = bridgeConnection(config.gatewayUrl)
  const idl = { ...ACRUX_BRIDGE_IDL, address: config.program.toBase58() } as unknown as Idl
  const coder = new BorshAccountsCoder(idl), program = new Program(idl, { connection: solana })
  let solanaConfirmedSlot = 0, solanaFinalizedSlot = 0
  const store = prismaKeeperStore(db)
  const journal = createJournal({ connection: solana, signer, store: pendingFinalityStore(store), isLeader, followMs: CONFIRM_MS, minContextSlot: () => solanaConfirmedSlot })
  const pda = (...seeds: Buffer[]) => PublicKey.findProgramAddressSync(seeds, config.program)[0]
  const setPda = (epoch: number) => pda(Buffer.from('set'), u64(epoch))
  const releasedPda = (sequence: number) => pda(Buffer.from('released'), u64(Math.floor(sequence / 8192)))
  const bridge = pda(Buffer.from('bridge'))
  const own = () => { if (!isLeader()) throw new Error('KEEPER_NOT_LEADER') }
  const programName = config.program.toBase58(), wallet = signer.publicKey.toBase58()
  let epochSlots = 0
  let chainId = Buffer.alloc(32)

  function decode<T>(name: string, info: AccountInfo<Buffer>): T {
    if (!info.owner.equals(config.program) || info.executable) throw new Error(`Invalid bridge ${name} owner.`)
    return coder.decode<T>(name, info.data)
  }
  async function account<T>(connection: Connection, address: PublicKey, name: string, commitment: Finality = 'confirmed'): Promise<T | null> {
    const config = connection === solana ? { commitment, minContextSlot: commitment === 'confirmed' ? solanaConfirmedSlot : solanaFinalizedSlot } : { commitment }
    const info = await connection.getAccountInfo(address, config)
    return info ? decode<T>(name, info) : null
  }
  async function isReleased(sequence: number) {
    const state = await account<{ bucket: BN; bits: number[] }>(solana, releasedPda(sequence), 'Released', 'finalized')
    if (!state) return false
    if (exact(state.bucket, 'released bucket') !== Math.floor(sequence / 8192)) throw new Error('Released bucket differs.')
    const bit = sequence % 8192
    return !!(state.bits[Math.floor(bit / 8)] & (1 << (bit % 8)))
  }
  async function programAccounts<T>(name: string, commitment: Finality = 'confirmed') {
    const accounts = await solana.getProgramAccounts(config.program, { commitment, minContextSlot: commitment === 'confirmed' ? solanaConfirmedSlot : solanaFinalizedSlot, filters: [{ memcmp: { offset: 0, bytes: coder.accountDiscriminator(name).toString('base64'), encoding: 'base64' } }] })
    return accounts.map(({ pubkey, account: info }) => ({ address: pubkey, value: decode<T>(name, info) }))
  }
  async function report(patch: { lastCblock?: bigint; lastHandedEpoch?: bigint; lastRelayedAt?: Date; nextWithdrawalSequence?: bigint } = {}) {
    own()
    const lamports = exact(await solana.getBalance(signer.publicKey, 'finalized'), 'relayer balance').toString()
    await db.bridgeRelayerState.upsert({ where: { program: programName }, create: { program: programName, wallet, lamports, ...patch }, update: { wallet, lamports, ...patch } })
  }
  async function proof(params: { consensusSlot: number } | { epoch: number } | { latest: true }) {
    const response = await fetch(config.gatewayUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getCblockProof', params }), signal: AbortSignal.timeout(RPC_MS) })
    if (!response.ok) throw new Error('Gateway proof request failed.')
    const body = await response.json() as { result?: unknown; error?: unknown }
    if (body.error || !body.result) throw new Error('Gateway does not hold the requested finalized proof.')
    const parsed = readProof(body.result, epochSlots, chainId)
    if ('consensusSlot' in params && parsed.consensusSlot !== params.consensusSlot || 'epoch' in params && Math.floor(parsed.consensusSlot * 4 / epochSlots) !== params.epoch) throw new Error('Gateway returned another cblock or epoch.')
    return parsed
  }
  const unpaidKey = (sequence: number) => ({ program_sequence: { program: programName, sequence: BigInt(sequence) } })
  function savedProof(proof: Proof): Prisma.InputJsonObject {
    return {
      consensusSlot: proof.consensusSlot, chainId: proof.chainId, epochSlots: proof.epochSlots,
      hash: proof.hash, outboxRoot: proof.outboxRoot, preimage: proof.preimage.toString('base64'),
      outbox: proof.outbox.map(entry => entry.toString('base64')), validators: proof.validators.map(key => key.toBase58()),
      stakes: proof.stakes, votes: proof.votes,
    }
  }
  async function rememberUnpaid(proof: Proof, sequence: number) {
    own()
    return db.bridgeUnpaidWithdrawal.upsert({
      where: unpaidKey(sequence),
      create: { program: programName, sequence: BigInt(sequence), cblock: BigInt(proof.consensusSlot), epoch: BigInt(Math.floor(proof.consensusSlot * 4 / epochSlots)), proof: savedProof(proof), reason: null, attempts: 0, nextAttemptAt: new Date() },
      update: {},
    })
  }
  async function forgetUnpaid(sequence: number) {
    own()
    await db.bridgeUnpaidWithdrawal.deleteMany({ where: { program: programName, sequence: BigInt(sequence) } })
  }
  async function deferRelease(proof: Proof, sequence: number, error: unknown) {
    own()
    const unpaid = await rememberUnpaid(proof, sequence)
    const attempts = unpaid.attempts + 1
    const nextAttemptAt = new Date(Date.now() + (attempts === 1 ? 2 : attempts === 2 ? 5 : 60) * 60_000)
    const failure = reason(error)
    await db.bridgeUnpaidWithdrawal.update({ where: unpaidKey(sequence), data: { reason: failure, attempts, nextAttemptAt } })
    log({ event: 'bridge_release_deferred', sequence, cblock: proof.consensusSlot, epoch: Math.floor(proof.consensusSlot * 4 / epochSlots), attempts, nextAttemptAt: nextAttemptAt.toISOString(), error: failure })
  }
  async function withdrawalProof(sequence: number) {
    const unpaid = await db.bridgeUnpaidWithdrawal.findUnique({ where: unpaidKey(sequence) })
    if (unpaid) return readProof(unpaid.proof, epochSlots, chainId)
    // Rows signed by the preceding version predate the unpaid queue; their Acrux record and
    // retained gateway proof supply the same durable retry material before they are followed.
    const withdrawal = await account<Withdrawal>(acrux, pda(Buffer.from('withdrawal'), u64(sequence)), 'Withdrawal', 'finalized')
    if (!withdrawal || exact(withdrawal.sequence, 'withdrawal sequence') !== sequence) throw new Error('Acrux withdrawal record is missing or inconsistent.')
    const found = await proof({ consensusSlot: Math.floor(exact(withdrawal.slot, 'withdrawal slot') / 4) })
    if (!found.entries.some(entry => entry.kind === 'withdrawal' && entry.sequence === sequence && entry.mint.equals(withdrawal.solana_mint) && entry.recipient.equals(withdrawal.recipient) && entry.amount.eq(withdrawal.amount))) throw new Error('Gateway outbox does not match the burned withdrawal.')
    await rememberUnpaid(found, sequence)
    return found
  }
  /** Journal confirmation is enough to chain instructions; only finalized receipts retire bytes. */
  async function receipt(row: Pick<KeeperTxRow, 'key' | 'signature'>, commitment: Finality) {
    const deadline = Date.now() + CONFIRM_MS
    while (Date.now() < deadline) {
      const status = (await solana.getSignatureStatuses([row.signature], { searchTransactionHistory: true })).value[0]
      if (status?.err && (status.confirmationStatus === 'confirmed' || status.confirmationStatus === 'finalized')) {
        await store.update(row.key, { state: 'failed', error: 'Bridge transaction failed on chain.' })
        throw new Error('Bridge transaction failed on chain.')
      }
      if (status?.confirmationStatus === 'confirmed' || status?.confirmationStatus === 'finalized') {
        solanaConfirmedSlot = Math.max(solanaConfirmedSlot, status.slot)
        if (status.confirmationStatus === 'finalized') {
          solanaFinalizedSlot = Math.max(solanaFinalizedSlot, status.slot)
          await store.update(row.key, { state: 'confirmed' })
        }
        if (commitment === 'confirmed' || status.confirmationStatus === 'finalized') return status
      }
      await pause(500)
    }
    throw new Error(`Bridge transaction ${commitment} deadline exceeded.`)
  }
  async function action(event: string, key: string, fields: Record<string, unknown>, instructions: () => Promise<TransactionInstruction[]>, applied: (commitment: Finality) => Promise<boolean>, commitment: Finality = 'confirmed') {
    const journalKey = `bridge:${programName}:${key}`
    try {
      own()
      let previous = await store.get(journalKey)
      // This task retries a landed failure on its own schedule. Preflight/transport ambiguity
      // remains signed and must follow the original bytes through confirmation or expiry.
      if (previous?.state === 'failed') {
        await store.update(journalKey, { state: 'expired' })
        previous = { ...previous, state: 'expired' }
      }
      let completed: KeeperTxRow | null = null
      let resumed = false
      if (previous?.state === 'confirmed') {
        // Reconcile even legacy rows first. Confirmation/absence alone never permits new bytes.
        await store.update(journalKey, { state: 'signed' })
        previous = { ...previous, state: 'signed' }
      }
      if (previous?.state === 'signed') {
        if (await journal.follow(journalKey) !== 'confirmed') throw new Error('Bridge transaction confirmation deadline exceeded.')
        const status = await receipt(previous, commitment)
        if (await applied(commitment)) {
          completed = previous
          resumed = true
        } else {
          // A finalized post/attest/close can need a new generation after its cblock was closed
          // and reposted. A paid release's permanent bit can never legitimately disappear.
          if (status.confirmationStatus !== 'finalized' || event === 'bridge_released') throw new Error('Bridge transaction effect is not visible; reconcile its original bytes next tick.')
          await store.update(journalKey, { state: 'expired' })
        }
      }
      if (!completed) {
        if (await journal.execute(journalKey, instructions) !== 'confirmed') throw new Error('Bridge transaction confirmation deadline exceeded.')
        completed = await store.get(journalKey)
        if (!completed) throw new Error('Bridge transaction journal is missing.')
        await receipt(completed, commitment)
        // Successful confirmed program execution establishes intermediate writes. Release alone
        // also checks its permanent bitmap at finality before reporting payment.
        if (event === 'bridge_released' && !await applied(commitment)) throw new Error('Finalized release bit is missing.')
      }
      log({ event, ...fields, signature: completed.signature, commitment, ...(resumed ? { resumed: true } : {}) })
    } catch (error) {
      log({ event: `${event}_failed`, ...fields, error: reason(error) })
      throw error
    }
    await report({ lastRelayedAt: new Date() })
  }
  async function certify(proof: Proof) {
    const hash = Buffer.from(proof.hash, 'hex'), cblock = pda(Buffer.from('cblock'), hash)
    const epoch = Math.floor(proof.consensusSlot * 4 / epochSlots)
    let state = await account<Cblock>(solana, cblock, 'Cblock')
    if (!state) {
      await action('bridge_cblock_posted', `post:${proof.hash}`, { cblock: proof.consensusSlot, epoch }, async () => [await program.methods.postCblock([...hash], [...Buffer.from(proof.outboxRoot, 'hex')], proof.preimage).accountsStrict({ payer: signer.publicKey, bridge, cblock, systemProgram: SystemProgram.programId }).instruction()], async commitment => !!await account<Cblock>(solana, cblock, 'Cblock', commitment))
      state = await account<Cblock>(solana, cblock, 'Cblock')
      if (!state) throw new Error('Posted bridge cblock is missing.')
    }
    if (!state.certified) {
      const set = await account<ValidatorSet>(solana, setPda(epoch), 'ValidatorSet')
      if (!set || set.validators.length !== proof.validators.length || set.validators.some((v, i) => !v.key.equals(proof.validators[i]) || v.stake.toString() !== proof.stakes[i])) throw new Error('Gateway validators differ from the certified validator set.')
      const { kind, batches } = attestationBatches(proof, state)
      // These transactions only OR distinct vote bits; none depends on another batch's write.
      const results = await Promise.allSettled(batches.map(batch => action('bridge_attested', `attest:${proof.hash}:${kind}:${batch.map(v => v.validator).join(',')}`, { cblock: proof.consensusSlot, epoch, kind, signatures: batch.length }, async () => [
        attestInstruction(proof, batch),
        await program.methods.attestCblock().accountsStrict({ bridge, cblock, set: setPda(epoch), instructions: SYSVAR_INSTRUCTIONS_PUBKEY }).instruction(),
      ], async commitment => {
        const counted = await account<Cblock>(solana, cblock, 'Cblock', commitment)
        return !!counted && (counted.certified || batch.every(vote => !!(counted[kind] >>> vote.validator & 1)))
      })))
      for (const result of results) if (result.status === 'rejected') throw result.reason
      state = await account<Cblock>(solana, cblock, 'Cblock')
      if (!state) throw new Error('Attested bridge cblock is missing.')
      if (!state.certified) throw new Error('Gateway proof does not carry enough stake to certify its cblock.')
    }
    await report({ lastCblock: BigInt(proof.consensusSlot) })
    return cblock
  }
  async function release(proof: Proof, entry: Extract<Entry, { kind: 'withdrawal' }>, index: number, cblock: PublicKey) {
    const unpaid = await rememberUnpaid(proof, entry.sequence)
    if (unpaid.nextAttemptAt.getTime() > Date.now()) return
    try {
      if (await isReleased(entry.sequence)) { await forgetUnpaid(entry.sequence); return }
      const mintInfo = await solana.getAccountInfo(entry.mint, { commitment: 'confirmed', minContextSlot: solanaConfirmedSlot })
      if (!mintInfo || ![TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID].some(id => id.equals(mintInfo.owner))) throw new Error('Withdrawal mint is not an SPL token mint.')
      unpackMint(entry.mint, mintInfo, mintInfo.owner)
      const tokenProgram = mintInfo.owner
      const destination = getAssociatedTokenAddressSync(entry.mint, entry.recipient, true, tokenProgram)
      await action('bridge_released', `release:${entry.sequence}`, { sequence: entry.sequence, cblock: proof.consensusSlot, amount: entry.amount.toString() }, async () => [
        createAssociatedTokenAccountIdempotentInstruction(signer.publicKey, destination, entry.recipient, entry.mint, tokenProgram),
        await program.methods.release(new BN(entry.sequence), entry.recipient, entry.amount, outboxProof(proof.outbox, index)).accountsStrict({ payer: signer.publicKey, cblock, mint: entry.mint, vault: pda(Buffer.from('vault'), entry.mint.toBuffer()), destination, released: releasedPda(entry.sequence), tokenProgram, systemProgram: SystemProgram.programId }).instruction(),
      ], async () => isReleased(entry.sequence), 'finalized')
      await forgetUnpaid(entry.sequence)
    } catch (error) {
      // Failed simulations and landed failures may rebuild later. An ambiguous signed row is
      // left untouched: the same journal bytes must settle or expire before any replacement.
      await deferRelease(proof, entry.sequence, error)
    }
  }
  async function relay(proof: Proof, handoffEpoch?: number) {
    const cblock = await certify(proof)
    // Each release has its own durable outcome and retry date; an unpayable recipient cannot
    // reject another payout sharing this proof or prevent the validator set from handing on.
    const results = await Promise.allSettled(proof.entries.map(async (entry, index) => {
      if (entry.kind === 'set') {
        if (entry.epoch !== handoffEpoch || await account<ValidatorSet>(solana, setPda(entry.epoch), 'ValidatorSet')) return
        await action('bridge_handed_on', `handoff:${entry.epoch}`, { epoch: entry.epoch, cblock: proof.consensusSlot }, async () => [await program.methods.handOff(new BN(entry.epoch), entry.validators, outboxProof(proof.outbox, index)).accountsStrict({ payer: signer.publicKey, cblock, next: setPda(entry.epoch), systemProgram: SystemProgram.programId }).instruction()], async commitment => !!await account<ValidatorSet>(solana, setPda(entry.epoch), 'ValidatorSet', commitment))
      } else {
        await release(proof, entry, index, cblock)
      }
    }))
    for (const result of results) if (result.status === 'rejected') throw result.reason
    // Cleanup runs after the complete scan, with every unpaid and not-yet-finalized withdrawal
    // accounted for, rather than closing the cblock while another payout still needs it.
  }
  async function closeCblock(cblock: PublicKey, slot: number) {
    await action('bridge_account_closed', `close-cblock:${cblock.toBase58()}`, { account: cblock.toBase58(), kind: 'cblock', cblock: slot }, async () => [await program.methods.closeCblock().accountsStrict({ payer: signer.publicKey, cblock }).instruction()], async commitment => !await account<Cblock>(solana, cblock, 'Cblock', commitment))
  }
  async function runCycle() {
    own()
    const state = await account<{ epoch_slots: BN; chain_id: number[] }>(solana, bridge, 'Bridge')
    if (!state) throw new Error('Initialize the bridge and seed its validator set before starting the relayer.')
    epochSlots = exact(state.epoch_slots, 'bridge epoch length')
    chainId = Buffer.from(state.chain_id)
    if (!epochSlots || epochSlots % 4) throw new Error('Invalid bridge epoch length.')
    await report()
    // Reconcile this task's signed bytes before account guards can skip an action that landed
    // during a restart. Never resume the buyback journal under the bridge's separate lease.
    const pending = await db.keeperTransaction.findMany({ where: { state: 'signed', key: { startsWith: `bridge:${programName}:` } }, orderBy: { createdAt: 'asc' } })
    const resumed = await Promise.allSettled(pending.map(async row => {
      const kind = row.key.split(':')[2]
      const event = ({ post: 'bridge_cblock_posted', attest: 'bridge_attested', handoff: 'bridge_handed_on', release: 'bridge_released', 'close-cblock': 'bridge_account_closed', 'close-set': 'bridge_account_closed' } as Record<string, string>)[kind]
      const commitment = kind === 'release' ? 'finalized' : 'confirmed'
      const sequence = kind === 'release' ? Number(row.key.split(':')[3]) : null
      let retained: Proof | null = null
      try {
        if (sequence !== null) {
          // The archive may already have pruned a paid withdrawal. Its permanent finalized bit
          // retires this never-reused release key without needing that old proof or receipt.
          if (await isReleased(sequence)) {
            own()
            await store.update(row.key, { state: 'confirmed' })
            await forgetUnpaid(sequence)
            log({ event, sequence, actionKey: row.key, signature: row.signature, commitment: 'finalized', resumed: true })
            return
          }
          const unpaid = await db.bridgeUnpaidWithdrawal.findUnique({ where: unpaidKey(sequence) })
          if (unpaid && unpaid.nextAttemptAt.getTime() > Date.now()) return
          retained = await withdrawalProof(sequence)
        }
        if (await journal.follow(row.key) !== 'confirmed') throw new Error('Bridge transaction confirmation deadline exceeded.')
        await receipt(row, commitment)
        if (sequence !== null) {
          if (!await isReleased(sequence)) throw new Error('Finalized release bit is missing.')
          await forgetUnpaid(sequence)
        }
        log({ event, actionKey: row.key, signature: row.signature, commitment, resumed: true })
      } catch (error) {
        log({ event: `${event}_failed`, actionKey: row.key, error: reason(error) })
        if (sequence === null) throw error
        if (!retained) {
          const unpaid = await db.bridgeUnpaidWithdrawal.findUnique({ where: unpaidKey(sequence) })
          if (unpaid) retained = readProof(unpaid.proof, epochSlots, chainId)
        }
        if (retained) await deferRelease(retained, sequence, error)
        // A legacy row may lack retry material while RPC is unavailable. Keep its signed bytes;
        // failure to recover that one proof must not prevent other pending payouts resuming.
        return
      }
      await report({ lastRelayedAt: new Date() })
    }))
    for (const result of resumed) if (result.status === 'rejected') throw result.reason
    const latest = await proof({ latest: true })
    const finalizedSlot = latest.consensusSlot * 4 + 3, currentEpoch = Math.floor(finalizedSlot / epochSlots)
    let sets = await programAccounts<ValidatorSet>('ValidatorSet')
    for (const set of sets) if (!set.address.equals(setPda(exact(set.value.epoch, 'validator epoch')))) throw new Error('Validator set PDA differs.')
    if (!sets.length) throw new Error('Seed the bridge validator set before starting the relayer.')
    let newestEpoch = Math.max(...sets.map(set => exact(set.value.epoch, 'validator epoch')))
    while (newestEpoch < currentEpoch) {
      await relay(await proof({ epoch: newestEpoch }), newestEpoch + 1)
      newestEpoch++
    }
    // Read the counter after the finalized frontier. Accounts are monotonic on Acrux; records
    // newer than that frontier are left for the next tick and forbid old-set cleanup this tick.
    const outbound = await account<{ withdrawals: BN }>(acrux, pda(Buffer.from('outbound')), 'Outbound', 'finalized')
    const count = outbound ? exact(outbound.withdrawals, 'withdrawal count') : 0
    const row = await db.bridgeRelayerState.findUniqueOrThrow({ where: { program: programName } })
    let next = exact(row.nextWithdrawalSequence, 'withdrawal cursor')
    let unscannedCblock: number | null = null
    let unscannedEpoch: number | null = null
    while (next < count) {
      const withdrawal = await account<Withdrawal>(acrux, pda(Buffer.from('withdrawal'), u64(next)), 'Withdrawal', 'finalized')
      if (!withdrawal || exact(withdrawal.sequence, 'withdrawal sequence') !== next) throw new Error('Acrux withdrawal record is missing or inconsistent.')
      if (exact(withdrawal.slot, 'withdrawal slot') > finalizedSlot) {
        unscannedCblock = Math.floor(exact(withdrawal.slot, 'withdrawal slot') / 4)
        unscannedEpoch = Math.floor(exact(withdrawal.slot, 'withdrawal slot') / epochSlots)
        break
      }
      if (!await isReleased(next)) {
        const cblock = Math.floor(exact(withdrawal.slot, 'withdrawal slot') / 4)
        const retained = await db.bridgeUnpaidWithdrawal.findUnique({ where: unpaidKey(next) })
        const burnProof = retained ? readProof(retained.proof, epochSlots, chainId) : await proof({ consensusSlot: cblock })
        if (!burnProof.entries.some(entry => entry.kind === 'withdrawal' && entry.sequence === next && entry.mint.equals(withdrawal.solana_mint) && entry.recipient.equals(withdrawal.recipient) && entry.amount.eq(withdrawal.amount))) throw new Error('Gateway outbox does not match the burned withdrawal.')
        await relay(burnProof)
      } else {
        await forgetUnpaid(next)
      }
      next++
      await report({ nextWithdrawalSequence: BigInt(next) })
    }
    // Retries use the saved proof even after the gateway's rolling proof has disappeared.
    const due = await db.bridgeUnpaidWithdrawal.findMany({ where: { program: programName, nextAttemptAt: { lte: new Date() } }, orderBy: { sequence: 'asc' } })
    const retries = new Map<string, { proof: Proof; entries: { entry: Extract<Entry, { kind: 'withdrawal' }>; index: number }[] }>()
    for (const unpaid of due) {
      const sequence = exact(unpaid.sequence, 'unpaid withdrawal sequence')
      const retained = readProof(unpaid.proof, epochSlots, chainId)
      const index = retained.entries.findIndex(entry => entry.kind === 'withdrawal' && entry.sequence === sequence)
      const entry = retained.entries[index]
      if (!entry || entry.kind !== 'withdrawal' || retained.consensusSlot !== exact(unpaid.cblock, 'unpaid cblock')) throw new Error('Saved withdrawal proof is inconsistent.')
      let group = retries.get(retained.hash)
      if (!group) {
        group = { proof: retained, entries: [] }
        retries.set(retained.hash, group)
      }
      group.entries.push({ entry, index })
    }
    const retried = await Promise.allSettled([...retries.values()].map(async group => {
      const checked = await Promise.allSettled(group.entries.map(async item => {
        try {
          if (await isReleased(item.entry.sequence)) { await forgetUnpaid(item.entry.sequence); return null }
          return item
        } catch (error) {
          await deferRelease(group.proof, item.entry.sequence, error)
          return null
        }
      }))
      const entries: typeof group.entries = []
      for (const check of checked) {
        if (check.status === 'rejected') throw check.reason
        if (check.value) entries.push(check.value)
      }
      if (!entries.length) return
      let cblock: PublicKey
      try { cblock = await certify(group.proof) }
      catch (error) {
        const deferred = await Promise.allSettled(entries.map(({ entry }) => deferRelease(group.proof, entry.sequence, error)))
        for (const result of deferred) if (result.status === 'rejected') throw result.reason
        return
      }
      const released = await Promise.allSettled(entries.map(({ entry, index }) => release(group.proof, entry, index, cblock)))
      for (const result of released) if (result.status === 'rejected') throw result.reason
    }))
    for (const result of retried) if (result.status === 'rejected') throw result.reason
    // Keep only the accounts needed by recorded unpaid or not-yet-scanned withdrawals. A failed
    // release in an old epoch must not stop unrelated cblocks and validator sets returning rent.
    const unpaid = await db.bridgeUnpaidWithdrawal.findMany({ where: { program: programName }, select: { cblock: true, epoch: true } })
    const keptCblocks = new Set(unpaid.map(row => exact(row.cblock, 'unpaid cblock')))
    const keptEpochs = new Set(unpaid.map(row => exact(row.epoch, 'unpaid epoch')))
    const cblocks = await programAccounts<Cblock>('Cblock')
    for (const cblock of cblocks) {
      if (!cblock.address.equals(pda(Buffer.from('cblock'), Buffer.from(cblock.value.hash)))) throw new Error('Cblock PDA differs.')
      const slot = exact(cblock.value.slot, 'cblock slot')
      if (keptCblocks.has(slot) || unscannedCblock !== null && slot >= unscannedCblock) continue
      if (cblock.value.payer.equals(signer.publicKey)) await closeCblock(cblock.address, slot)
    }
    sets = await programAccounts<ValidatorSet>('ValidatorSet', 'finalized')
    const newestFinalizedEpoch = sets.length ? Math.max(...sets.map(set => exact(set.value.epoch, 'validator epoch'))) : null
    for (const set of sets) {
      const epoch = exact(set.value.epoch, 'validator epoch')
      if (newestFinalizedEpoch === null || epoch + 1 >= newestFinalizedEpoch || epoch >= currentEpoch || keptEpochs.has(epoch) || unscannedEpoch !== null && epoch >= unscannedEpoch || !set.value.payer.equals(signer.publicKey)) continue
      await action('bridge_account_closed', `close-set:${epoch}`, { account: set.address.toBase58(), kind: 'validator_set', epoch }, async () => [await program.methods.closeValidatorSet().accountsStrict({ payer: signer.publicKey, set: set.address, newest: setPda(newestFinalizedEpoch) }).instruction()], async commitment => !await account<ValidatorSet>(solana, set.address, 'ValidatorSet', commitment))
    }
    await report(newestFinalizedEpoch === null ? {} : { lastHandedEpoch: BigInt(newestFinalizedEpoch) })
  }
  return { runCycle }
}