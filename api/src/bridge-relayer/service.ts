import { ACRUX_BRIDGE_IDL, BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING } from '@acrux/contract'
import { BorshAccountsCoder, Program, type Idl } from '@coral-xyz/anchor'
import BN from 'bn.js'
import type { Prisma, PrismaClient } from '@prisma/client'
import { Connection, PublicKey, SystemProgram, SYSVAR_INSTRUCTIONS_PUBKEY, VersionedTransaction, type AccountInfo, type Keypair, type TransactionInstruction } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, getAssociatedTokenAddressSync, unpackAccount, unpackMint } from '@solana/spl-token'
import { createJournal, type KeeperTxRow } from '../engines/keeper/journal.js'
import { prismaKeeperStore } from '../engines/keeper/prisma-store.js'
import type { BridgeRelayerConfig } from './config.js'
import { attestInstruction, exact, outboxProof, readProof, u64, type Entry, type Proof } from './proof.js'
import { attestationBatches, pendingFinalityStore } from './transactions.js'

/** An error's message for the log, with any URL (which may carry an RPC key) left out. */
export const reason = (error: unknown) => error instanceof Error ? `${error.name}: ${error.message.replace(/https?:\/\/\S+/g, '<url>')}` : 'Error'
const RPC_MS = 10_000
const CONFIRM_MS = 30_000
// About one Released bucket's rent and a cblock post: below it a payout cannot land, so its
// withdrawals are retried each minute and pay within a minute of the relayer being funded.
const UNFUNDED_LAMPORTS = 10_000_000
const RETIRED_ACCOUNT_CREATION = 'The previous recipient-account creation transaction is retired. Waiting for its blockhash to expire.'
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))

// A load-balanced RPC can route a read to a node behind the slot the last receipt came from.
// That node catches up within a second or two, so the request is asked again instead of
// failing the whole cycle.
const MIN_CONTEXT_SLOT_NOT_REACHED = '"code":-32016'
const MIN_CONTEXT_ATTEMPTS = 10

/** HTTP-only connections: every request is aborted; confirmation uses the keeper journal's bounded poll. */
export function bridgeConnection(url: string) {
  return new Connection(url, { commitment: 'confirmed', disableRetryOnRateLimit: true, fetch: async (input, init) => {
    for (let attempt = 1; ; attempt++) {
      const response = await fetch(input, {
        ...init, signal: init?.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(RPC_MS)]) : AbortSignal.timeout(RPC_MS),
      })
      const body = await response.text()
      if (attempt === MIN_CONTEXT_ATTEMPTS || !body.includes(MIN_CONTEXT_SLOT_NOT_REACHED)) return new Response(body, { status: response.status, statusText: response.statusText })
      await pause(500)
    }
  } })
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
  const journal = createJournal({
    connection: solana, signer, store: pendingFinalityStore(store), isLeader, followMs: CONFIRM_MS, minContextSlot: () => solanaConfirmedSlot,
    beforeBroadcast: bytes => {
      const message = VersionedTransaction.deserialize(bytes).message
      if (message.compiledInstructions.some(instruction => message.staticAccountKeys[instruction.programIdIndex]?.equals(ASSOCIATED_TOKEN_PROGRAM_ID))) throw new Error(RETIRED_ACCOUNT_CREATION)
    },
  })
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
  type ReleasedBucket = { bucket: BN; bits: number[] }
  async function releasedBucket(bucket: number) {
    const state = await account<ReleasedBucket>(solana, pda(Buffer.from('released'), u64(bucket)), 'Released', 'finalized')
    if (state && exact(state.bucket, 'released bucket') !== bucket) throw new Error('Released bucket differs.')
    return state
  }
  async function isReleased(sequence: number) {
    const state = await releasedBucket(Math.floor(sequence / 8192))
    const bit = sequence % 8192
    return !!state && !!(state.bits[Math.floor(bit / 8)] & (1 << (bit % 8)))
  }
  async function programAccounts<T>(name: string, commitment: Finality = 'confirmed') {
    const accounts = await solana.getProgramAccounts(config.program, { commitment, minContextSlot: commitment === 'confirmed' ? solanaConfirmedSlot : solanaFinalizedSlot, filters: [{ memcmp: { offset: 0, bytes: coder.accountDiscriminator(name).toString('base64'), encoding: 'base64' } }] })
    return accounts.map(({ pubkey, account: info }) => ({ address: pubkey, value: decode<T>(name, info) }))
  }
  let balance = Number.POSITIVE_INFINITY
  async function report(patch: { lastCblock?: bigint; lastHandedEpoch?: bigint; lastRelayedAt?: Date; nextWithdrawalSequence?: bigint } = {}) {
    own()
    balance = exact(await solana.getBalance(signer.publicKey, 'finalized'), 'relayer balance')
    const lamports = balance.toString()
    await db.bridgeRelayerState.upsert({ where: { program: programName }, create: { program: programName, wallet, lamports, ...patch }, update: { wallet, lamports, ...patch } })
  }
  type ProofSelector = { consensusSlot: number } | { epoch: number } | { latest: true }
  function proof(params: { feeBurnEpoch: number }): Promise<Proof | null>
  function proof(params: ProofSelector): Promise<Proof>
  async function proof(params: ProofSelector | { feeBurnEpoch: number }): Promise<Proof | null> {
    const response = await fetch(config.gatewayUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getCblockProof', params }), signal: AbortSignal.timeout(RPC_MS) })
    if (!response.ok) throw new Error('Gateway proof request failed.')
    const body = await response.json() as { result?: unknown; error?: unknown }
    if (body.error || body.result === undefined) throw new Error('Gateway does not hold the requested finalized proof.')
    if (body.result === null) {
      if ('feeBurnEpoch' in params) return null
      throw new Error('Gateway does not hold the requested finalized proof.')
    }
    const parsed = readProof(body.result, epochSlots, chainId)
    if ('consensusSlot' in params && parsed.consensusSlot !== params.consensusSlot || 'epoch' in params && Math.floor(parsed.consensusSlot * 4 / epochSlots) !== params.epoch) throw new Error('Gateway returned another cblock or epoch.')
    if ('feeBurnEpoch' in params) {
      const expected = (params.feeBurnEpoch + 1) * (epochSlots / 4) - 1
      if (!Number.isSafeInteger(expected) || parsed.consensusSlot !== expected) throw new Error('Gateway returned another fee burn epoch ending.')
    }
    return parsed
  }
  async function acknowledgeSavedCursor(nextWithdrawalSequence: number) {
    own()
    const response = await fetch(new URL('/internal/bridge-saved', config.gatewayInternalUrl), {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ program: programName, nextWithdrawalSequence: String(nextWithdrawalSequence) }), signal: AbortSignal.timeout(RPC_MS),
    })
    if (!response.ok) throw new Error('Reporting the saved bridge cursor failed.')
    const body = await response.json() as { program?: unknown; nextWithdrawalSequence?: unknown }
    if (body.program !== programName || typeof body.nextWithdrawalSequence !== 'string' || !/^(0|[1-9]\d*)$/.test(body.nextWithdrawalSequence)
      || BigInt(body.nextWithdrawalSequence) > 18_446_744_073_709_551_615n || BigInt(body.nextWithdrawalSequence) < BigInt(nextWithdrawalSequence)) throw new Error('The node returned an invalid saved bridge cursor.')
    log({ event: 'bridge_saved_cursor_reported', nextWithdrawalSequence })
  }
  const unpaidKey = (sequence: number) => ({ program_sequence: { program: programName, sequence: BigInt(sequence) } })
  function savedProof(proof: Proof): Prisma.InputJsonObject {
    return {
      consensusSlot: proof.consensusSlot, chainId: proof.chainId, epochSlots: proof.epochSlots,
      feeBurnFromSlot: proof.feeBurnFromSlot ?? null, admissionFromSlot: proof.admissionFromSlot ?? null, nativeMint: proof.nativeMint ?? null,
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
    const key = `bridge:${programName}:release:${sequence}`
    if ((await store.get(key))?.state === 'signed') await store.update(key, { state: 'confirmed' })
    await db.bridgeUnpaidWithdrawal.deleteMany({ where: { program: programName, sequence: BigInt(sequence) } })
  }
  async function deferRelease(proof: Proof, sequence: number, error: unknown) {
    own()
    const unpaid = await rememberUnpaid(proof, sequence)
    const attempts = unpaid.attempts + 1
    const missing = error instanceof Error && error.message === BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING
    const retired = error instanceof Error && error.message === RETIRED_ACCOUNT_CREATION
    const unfunded = balance < UNFUNDED_LAMPORTS
    const nextAttemptAt = new Date(Date.now() + (missing || retired || unfunded ? 1 : attempts === 1 ? 2 : attempts === 2 ? 5 : 60) * 60_000)
    const failure = missing ? BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING : reason(error)
    await db.bridgeUnpaidWithdrawal.update({ where: unpaidKey(sequence), data: { reason: failure, attempts, nextAttemptAt } })
    log({ event: 'bridge_release_deferred', sequence, cblock: proof.consensusSlot, epoch: Math.floor(proof.consensusSlot * 4 / epochSlots), attempts, nextAttemptAt: nextAttemptAt.toISOString(), error: failure })
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
          // and reposted. A completed money action's permanent bit cannot disappear.
          if (status.confirmationStatus !== 'finalized' || (event === 'bridge_released' || event === 'bridge_fee_burned')) throw new Error('Bridge transaction effect is not visible; reconcile its original bytes next tick.')
          await store.update(journalKey, { state: 'expired' })
        }
      }
      if (!completed) {
        if (await journal.execute(journalKey, instructions) !== 'confirmed') throw new Error('Bridge transaction confirmation deadline exceeded.')
        completed = await store.get(journalKey)
        if (!completed) throw new Error('Bridge transaction journal is missing.')
        await receipt(completed, commitment)
        // Confirmed execution establishes intermediate writes; both money actions also check
        // their permanent bitmap at finality before being reported complete.
        if ((event === 'bridge_released' || event === 'bridge_fee_burned') && !await applied(commitment)) throw new Error('Finalized bridge replay bit is missing.')
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
  // A validated mint's token-program owner is stable. Share its first lookup across entries;
  // a failed RPC or invalid account is never cached, so a later scheduled attempt can check it.
  const mintPrograms = new Map<string, Promise<PublicKey>>()
  async function mintTokenProgram(mint: PublicKey) {
    const key = mint.toBase58()
    let pending = mintPrograms.get(key)
    if (!pending) {
      pending = (async () => {
        const info = await solana.getAccountInfo(mint, { commitment: 'confirmed', minContextSlot: solanaConfirmedSlot })
        if (!info || info.executable || ![TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID].some(id => id.equals(info.owner)) || !unpackMint(mint, info, info.owner).isInitialized) throw new Error('Withdrawal mint is not an initialized SPL token mint.')
        return info.owner
      })()
      mintPrograms.set(key, pending)
    }
    try { return await pending }
    catch (error) {
      if (mintPrograms.get(key) === pending) mintPrograms.delete(key)
      throw error
    }
  }
  type RecipientTokenAccount = { destination: PublicKey; tokenProgram: PublicKey }
  async function recipientTokenAccount(entry: Extract<Entry, { kind: 'withdrawal' }>): Promise<RecipientTokenAccount> {
    const tokenProgram = await mintTokenProgram(entry.mint)
    const destination = getAssociatedTokenAddressSync(entry.mint, entry.recipient, true, tokenProgram)
    const info = await solana.getAccountInfo(destination, { commitment: 'confirmed', minContextSlot: solanaConfirmedSlot })
    if (!info || info.owner.equals(SystemProgram.programId) && info.data.length === 0) throw new Error(BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING)
    if (info.executable) throw new Error('Recipient token account is executable.')
    const account = unpackAccount(destination, info, tokenProgram)
    if (!account.owner.equals(entry.recipient) || !account.mint.equals(entry.mint) || !account.isInitialized) throw new Error('Recipient token account belongs to another owner or mint, or is not initialized.')
    return { destination, tokenProgram }
  }
  async function release(proof: Proof, entry: Extract<Entry, { kind: 'withdrawal' }>, index: number, cblock: PublicKey, { destination, tokenProgram }: RecipientTokenAccount) {
    const unpaid = await rememberUnpaid(proof, entry.sequence)
    if (unpaid.nextAttemptAt.getTime() > Date.now()) return
    try {
      if (await isReleased(entry.sequence)) { await forgetUnpaid(entry.sequence); return }
      // The destination was checked before certifying the cblock or following old bytes.
      // The journal also prevents retired account-creation bytes from being rebroadcast.
      await action('bridge_released', `release:${entry.sequence}`, { sequence: entry.sequence, cblock: proof.consensusSlot, amount: entry.amount.toString() }, async () => [
        await program.methods.release(new BN(entry.sequence), entry.recipient, entry.amount, outboxProof(proof.outbox, index)).accountsStrict({ payer: signer.publicKey, cblock, mint: entry.mint, vault: pda(Buffer.from('vault'), entry.mint.toBuffer()), destination, released: releasedPda(entry.sequence), tokenProgram, systemProgram: SystemProgram.programId }).instruction(),
      ], async () => isReleased(entry.sequence), 'finalized')
      await forgetUnpaid(entry.sequence)
    } catch (error) {
      // Failed simulations and landed failures may rebuild later. An ambiguous signed row is
      // left untouched: the same journal bytes must settle or expire before any replacement.
      await deferRelease(proof, entry.sequence, error)
    }
  }
  async function handOff(proof: Proof, epoch: number) {
    const cblock = await certify(proof)
    const index = proof.entries.findIndex(entry => entry.kind === 'set' && entry.epoch === epoch)
    const entry = proof.entries[index]
    if (!entry || entry.kind !== 'set') throw new Error('Gateway outbox does not contain the next validator epoch.')
    if (await account<ValidatorSet>(solana, setPda(epoch), 'ValidatorSet')) return
    await action('bridge_handed_on', `handoff:${epoch}`, { epoch, cblock: proof.consensusSlot }, async () => [await program.methods.handOff(new BN(epoch), entry.validators, outboxProof(proof.outbox, index)).accountsStrict({ payer: signer.publicKey, cblock, next: setPda(epoch), systemProgram: SystemProgram.programId }).instruction()], async commitment => !!await account<ValidatorSet>(solana, setPda(epoch), 'ValidatorSet', commitment))
  }
  async function isFeeBurned(epoch: number) {
    const bucket = Math.floor(epoch / 8192)
    const info = await solana.getAccountInfo(pda(Buffer.from('fee_burn'), u64(bucket)), { commitment: 'finalized', minContextSlot: solanaFinalizedSlot })
    // Anchor can initialize a PDA that someone has already funded as an empty system account.
    if (!info || !info.executable && info.owner.equals(SystemProgram.programId) && info.data.length === 0) return false
    const state = decode<ReleasedBucket>('Burned', info)
    if (exact(state.bucket, 'fee burn bucket') !== bucket) throw new Error('Fee burn bucket differs.')
    const bit = epoch % 8192
    return !!(state.bits[Math.floor(bit / 8)] & (1 << (bit % 8)))
  }
  async function retireFeeBurn(epoch: number) {
    own()
    const key = `bridge:${programName}:fee-burn:${epoch}`
    if ((await store.get(key))?.state === 'signed') await store.update(key, { state: 'confirmed' })
  }
  async function burnFee(proof: Proof, nativeMint: PublicKey) {
    const index = proof.entries.findIndex(entry => entry.kind === 'feeBurn')
    const entry = proof.entries[index]
    const epoch = Math.floor(proof.consensusSlot * 4 / epochSlots)
    // A zero-fee epoch has no FeeBurn leaf. Its ending cblock still hands the set on.
    if (!await account<ValidatorSet>(solana, setPda(epoch + 1), 'ValidatorSet')) await handOff(proof, epoch + 1)
    if (!entry || entry.kind !== 'feeBurn' || entry.amount.isZero()) return
    const config = pda(Buffer.from('fee_burn_config'))
    const bound = await account<{ mint: PublicKey }>(solana, config, 'FeeBurnConfig')
    if (!bound || !bound.mint.equals(nativeMint)) throw new Error('Initialize the fee burn configuration with the native Solana mint before relaying burns.')
    const tokenProgram = await mintTokenProgram(nativeMint)
    const cblock = await certify(proof)
    await action('bridge_fee_burned', `fee-burn:${entry.epoch}`, { epoch: entry.epoch, amount: entry.amount.toString() }, async () => [
      await program.methods.burn(new BN(entry.epoch), entry.amount, outboxProof(proof.outbox, index)).accountsStrict({ payer: signer.publicKey, bridge, config, cblock, mint: nativeMint, vault: pda(Buffer.from('vault'), nativeMint.toBuffer()), burned: pda(Buffer.from('fee_burn'), u64(Math.floor(entry.epoch / 8192))), tokenProgram, systemProgram: SystemProgram.programId }).instruction(),
    ], async () => isFeeBurned(entry.epoch), 'finalized')
    await retireFeeBurn(entry.epoch)
  }
  type PayoutGroup = { proof: Proof; entries: { entry: Extract<Entry, { kind: 'withdrawal' }>; index: number }[] }
  async function paySelected(groups: Map<string, PayoutGroup>) {
    const results = await Promise.allSettled([...groups.values()].map(async group => {
      // Missing or unusable destinations are parked before any cblock rent or signatures.
      const checked = await Promise.allSettled(group.entries.map(async target => {
        try { return { ...target, account: await recipientTokenAccount(target.entry) } }
        catch (error) { await deferRelease(group.proof, target.entry.sequence, error); return null }
      }))
      const eligible: { entry: Extract<Entry, { kind: 'withdrawal' }>; index: number; account: RecipientTokenAccount }[] = []
      for (const result of checked) {
        if (result.status === 'rejected') throw result.reason
        if (result.value) eligible.push(result.value)
      }
      if (!eligible.length) return
      let cblock: PublicKey
      try { cblock = await certify(group.proof) }
      catch (error) {
        const deferred = await Promise.allSettled(eligible.map(({ entry }) => deferRelease(group.proof, entry.sequence, error)))
        for (const result of deferred) if (result.status === 'rejected') throw result.reason
        return
      }
      const releases = await Promise.allSettled(eligible.map(({ entry, index, account }) => release(group.proof, entry, index, cblock, account)))
      for (const result of releases) if (result.status === 'rejected') throw result.reason
    }))
    for (const result of results) if (result.status === 'rejected') throw result.reason
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
    const latest = await proof({ latest: true })
    const finalizedSlot = latest.consensusSlot * 4 + 3, currentEpoch = Math.floor(finalizedSlot / epochSlots)
    let sets = await programAccounts<ValidatorSet>('ValidatorSet')
    for (const set of sets) if (!set.address.equals(setPda(exact(set.value.epoch, 'validator epoch')))) throw new Error('Validator set PDA differs.')
    if (!sets.length) throw new Error('Seed the bridge validator set before starting the relayer.')
    const row = await db.bridgeRelayerState.findUniqueOrThrow({ where: { program: programName } })
    const firstFeeBurnEpoch = latest.feeBurnFromSlot == null ? null : latest.feeBurnFromSlot / epochSlots
    const nativeMint = firstFeeBurnEpoch === null ? null : new PublicKey(latest.nativeMint!)
    let nextFeeBurnEpoch: number | null = null
    if (firstFeeBurnEpoch !== null) {
      nextFeeBurnEpoch = row.nextFeeBurnEpoch == null ? firstFeeBurnEpoch : exact(row.nextFeeBurnEpoch, 'fee burn cursor')
      if (nextFeeBurnEpoch < firstFeeBurnEpoch) throw new Error('The fee burn rule moved beyond its persisted cursor.')
      if (row.nextFeeBurnEpoch == null) {
        own()
        await db.bridgeRelayerState.update({ where: { program: programName }, data: { nextFeeBurnEpoch: BigInt(nextFeeBurnEpoch) } })
      }
    } else if (row.nextFeeBurnEpoch != null) throw new Error('The gateway removed the configured fee burn rule.')
    const ends = new Map<number, Proof | null>()
    const epochEnd = async (epoch: number) => {
      if (!ends.has(epoch)) ends.set(epoch, await proof({ feeBurnEpoch: epoch }))
      return ends.get(epoch)!
    }
    let newestEpoch = Math.max(...sets.map(set => exact(set.value.epoch, 'validator epoch')))
    // Admission freezes the next set halfway through this epoch. Hand it on while the
    // certifying epoch is still current, so its first new-epoch vote is already verifiable.
    const preHandNext = latest.admissionFromSlot != null && latest.consensusSlot * 4 >= latest.admissionFromSlot
      && latest.entries.some(entry => entry.kind === 'set' && entry.epoch === currentEpoch + 1)
    const handThroughEpoch = currentEpoch + Number(preHandNext)
    while (newestEpoch < handThroughEpoch) {
      const relay = newestEpoch === currentEpoch ? latest : firstFeeBurnEpoch !== null && newestEpoch >= firstFeeBurnEpoch ? await epochEnd(newestEpoch) ?? await proof({ epoch: newestEpoch }) : await proof({ epoch: newestEpoch })
      await handOff(relay, newestEpoch + 1)
      newestEpoch++
    }
    let burnFailure: { error: unknown } | null = null
    if (nextFeeBurnEpoch !== null) {
      let cursor: number = nextFeeBurnEpoch
      const completedThrough = Math.floor((finalizedSlot + 1) / epochSlots) - 1
      try {
        while (cursor <= completedThrough) {
          if (await isFeeBurned(cursor)) await retireFeeBurn(cursor)
          else {
            const end = await epochEnd(cursor)
            if (end) await burnFee(end, nativeMint!)
          }
          // An explicit null end carries unallocated fees forward. An end without FeeBurn has
          // no burn transaction; a positive leaf reaches here only after its permanent bit is final.
          const advanced = cursor + 1
          own()
          await db.bridgeRelayerState.update({ where: { program: programName }, data: { nextFeeBurnEpoch: BigInt(advanced) } })
          cursor = advanced
        }
      } catch (error) {
        burnFailure = { error }
        log({ event: 'bridge_fee_burn_deferred', epoch: cursor, error: reason(error) })
      }
      nextFeeBurnEpoch = cursor
    }
    const outbound = await account<{ withdrawals: BN }>(acrux, pda(Buffer.from('outbound')), 'Outbound', 'finalized')
    const count = outbound ? exact(outbound.withdrawals, 'withdrawal count') : 0
    let next = exact(row.nextWithdrawalSequence, 'withdrawal cursor')
    let saved = exact(row.savedWithdrawalSequence, 'saved withdrawal cursor')
    if (saved > count || next > count) throw new Error('Acrux withdrawal count is behind the persisted bridge cursors.')
    // The save pass advances by whole cblocks, independently of the much smaller pay budget.
    // Batch record reads and retain only the current batch while a large backlog is scanned.
    let recordStart = -1
    let records: (Withdrawal | null)[] = []
    const withdrawalRecord = async (sequence: number): Promise<Withdrawal> => {
      if (sequence < recordStart || sequence >= recordStart + records.length) {
        recordStart = sequence
        const sequences = Array.from({ length: Math.min(100, count - sequence) }, (_, index) => sequence + index)
        const found = await acrux.getMultipleAccountsInfo(sequences.map(value => pda(Buffer.from('withdrawal'), u64(value))), 'finalized')
        records = found.map(info => info ? decode<Withdrawal>('Withdrawal', info) : null)
      }
      const withdrawal = records[sequence - recordStart]
      if (!withdrawal || exact(withdrawal.sequence, 'withdrawal sequence') !== sequence) throw new Error('Acrux withdrawal record is missing or inconsistent.')
      return withdrawal
    }
    const historicReleases = new Map<number, ReleasedBucket | null>()
    const alreadyPaid = async (sequence: number) => {
      const bucket = Math.floor(sequence / 8192)
      if (!historicReleases.has(bucket)) historicReleases.set(bucket, await releasedBucket(bucket))
      const bitmap = historicReleases.get(bucket)
      const bit = sequence % 8192
      return !!bitmap && !!(bitmap.bits[Math.floor(bit / 8)] & (1 << (bit % 8)))
    }
    let visitedCblocks = 0
    let visited: number | null = null
    while (saved < count) {
      const withdrawal = await withdrawalRecord(saved)
      const slot = exact(withdrawal.slot, 'withdrawal slot')
      if (slot > finalizedSlot) break
      const cblock = Math.floor(slot / 4)
      if (cblock !== visited) {
        if (visitedCblocks >= config.maxSavedCblocksPerCycle) break
        visitedCblocks++
        visited = cblock
      }
      if (saved < next && await alreadyPaid(saved)) {
        // Before this cursor existed, finalized payments could have their gateway proof pruned.
        // Such records need no copy; all still-unpaid records below the cursor must have one.
        let through = saved + 1
        while (through < next && through < count) {
          const following = await withdrawalRecord(through)
          if (Math.floor(exact(following.slot, 'withdrawal slot') / 4) !== cblock || !await alreadyPaid(through)) break
          through++
        }
        own()
        await db.bridgeRelayerState.update({ where: { program: programName }, data: { savedWithdrawalSequence: BigInt(through) } })
        saved = through
        continue
      }
      const key = { program_cblock: { program: programName, cblock: BigInt(cblock) } }
      const existing = await db.bridgeSavedCblockProof.findUnique({ where: key })
      const parked = !existing && saved < next ? await db.bridgeUnpaidWithdrawal.findUnique({ where: unpaidKey(saved) }) : null
      const retained = existing ? readProof(existing.proof, epochSlots, chainId) : parked ? readProof(parked.proof, epochSlots, chainId) : await proof({ consensusSlot: cblock })
      const entries = retained.entries.filter((entry): entry is Extract<Entry, { kind: 'withdrawal' }> => entry.kind === 'withdrawal').sort((a, b) => a.sequence - b.sequence)
      const first = entries.find(entry => entry.sequence === saved)
      if (retained.consensusSlot !== cblock || !first || !first.mint.equals(withdrawal.solana_mint) || !first.recipient.equals(withdrawal.recipient) || !first.amount.eq(withdrawal.amount)) throw new Error('Gateway outbox does not match the burned withdrawal.')
      let through = saved
      for (const entry of entries.filter(entry => entry.sequence >= saved)) {
        if (entry.sequence !== through || entry.sequence >= count) throw new Error('Withdrawal proof sequences are not contiguous with the saved cursor.')
        through++
      }
      own()
      await db.$transaction(async tx => {
        await tx.bridgeSavedCblockProof.upsert({ where: key, create: { program: programName, cblock: BigInt(cblock), proof: savedProof(retained) }, update: {} })
        own()
        await tx.bridgeRelayerState.update({ where: { program: programName }, data: { savedWithdrawalSequence: BigInt(through) } })
      })
      saved = through
      log({ event: 'bridge_proof_saved', cblock, nextWithdrawalSequence: saved })
    }
    // Read the committed value back. Node restarts forget their in-memory report, so repeat it
    // every cycle, even when no new proof was saved. Never acknowledge an uncommitted copy.
    const committed = await db.bridgeRelayerState.findUniqueOrThrow({ where: { program: programName } })
    await acknowledgeSavedCursor(exact(committed.savedWithdrawalSequence, 'saved withdrawal cursor'))
    // New payment work has its own sequence-position budget; already-paid records consume it.
    let unscannedCblock: number | null = null
    let unscannedEpoch: number | null = null
    const fresh = new Map<string, PayoutGroup>()
    const proofCache = new Map<number, Proof>()
    let scanned = 0
    while (next < saved && scanned < config.maxNewWithdrawalsPerCycle) {
      const withdrawal = await withdrawalRecord(next)
      if (exact(withdrawal.slot, 'withdrawal slot') > finalizedSlot) break
      if (!await isReleased(next)) {
        const retained = await db.bridgeUnpaidWithdrawal.findUnique({ where: unpaidKey(next) })
        if (!retained || retained.nextAttemptAt.getTime() <= Date.now()) {
          const cblock = Math.floor(exact(withdrawal.slot, 'withdrawal slot') / 4)
          let burnProof = proofCache.get(cblock)
          if (!burnProof) {
            const stored = await db.bridgeSavedCblockProof.findUnique({ where: { program_cblock: { program: programName, cblock: BigInt(cblock) } } })
            if (!stored) throw new Error('A saved withdrawal proof is missing from Postgres.')
            burnProof = readProof(stored.proof, epochSlots, chainId)
            proofCache.set(cblock, burnProof)
          }
          const index = burnProof.entries.findIndex(entry => entry.kind === 'withdrawal' && entry.sequence === next && entry.mint.equals(withdrawal.solana_mint) && entry.recipient.equals(withdrawal.recipient) && entry.amount.eq(withdrawal.amount))
          const entry = burnProof.entries[index]
          if (!entry || entry.kind !== 'withdrawal' || burnProof.consensusSlot !== cblock) throw new Error('Gateway outbox does not match the burned withdrawal.')
          // Save the proof before moving the scan cursor. A restart then finds this exact target
          // in the due queue; neighbors in the proof never bypass either payout budget.
          await rememberUnpaid(burnProof, next)
          let group = fresh.get(burnProof.hash)
          if (!group) { group = { proof: burnProof, entries: [] }; fresh.set(burnProof.hash, group) }
          group.entries.push({ entry, index })
        }
      } else {
        await forgetUnpaid(next)
      }
      next++
      scanned++
      await report({ nextWithdrawalSequence: BigInt(next) })
    }
    if (next < count) {
      // One frontier read protects the unscanned suffix when the batch budget is exhausted.
      const withdrawal = await withdrawalRecord(next)
      unscannedCblock = Math.floor(exact(withdrawal.slot, 'withdrawal slot') / 4)
      unscannedEpoch = Math.floor(exact(withdrawal.slot, 'withdrawal slot') / epochSlots)
    }
    await paySelected(fresh)
    // Oldest due dates win, with the sequence providing a stable tie-break. Signed releases are
    // reconciled only as selected new/due targets, so pending journal rows cannot bypass the cap.
    // Rows waiting for a recipient's token account have a budget of their own, so many of them
    // never push back the retry of any other unpaid withdrawal.
    const now = new Date(), order = [{ nextAttemptAt: 'asc' as const }, { sequence: 'asc' as const }]
    const due = [...await db.bridgeUnpaidWithdrawal.findMany({
      where: { program: programName, nextAttemptAt: { lte: now }, OR: [{ reason: null }, { reason: { not: BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING } }] },
      orderBy: order, take: config.maxRetryWithdrawalsPerCycle,
    }), ...await db.bridgeUnpaidWithdrawal.findMany({
      where: { program: programName, nextAttemptAt: { lte: now }, reason: BRIDGE_RECIPIENT_TOKEN_ACCOUNT_MISSING },
      orderBy: order, take: config.maxAccountRetryWithdrawalsPerCycle,
    })]
    const retries = new Map<string, PayoutGroup>()
    const selected = await Promise.allSettled(due.map(async unpaid => {
      const sequence = exact(unpaid.sequence, 'unpaid withdrawal sequence')
      const retained = readProof(unpaid.proof, epochSlots, chainId)
      const index = retained.entries.findIndex(entry => entry.kind === 'withdrawal' && entry.sequence === sequence)
      const entry = retained.entries[index]
      if (!entry || entry.kind !== 'withdrawal' || retained.consensusSlot !== exact(unpaid.cblock, 'unpaid cblock')) throw new Error('Saved withdrawal proof is inconsistent.')
      try {
        if (await isReleased(sequence)) { await forgetUnpaid(sequence); return null }
        return { proof: retained, entry, index }
      } catch (error) {
        await deferRelease(retained, sequence, error)
        return null
      }
    }))
    for (const result of selected) {
      if (result.status === 'rejected') throw result.reason
      if (!result.value) continue
      const { proof: retained, entry, index } = result.value
      let group = retries.get(retained.hash)
      if (!group) { group = { proof: retained, entries: [] }; retries.set(retained.hash, group) }
      group.entries.push({ entry, index })
    }
    await paySelected(retries)
    // Retire only a bounded number of intermediate receipts, after all payout phases. Actions
    // required by a handoff recover their own original bytes before this housekeeping runs.
    const pending = await db.keeperTransaction.findMany({
      where: { state: 'signed', key: { startsWith: `bridge:${programName}:`, not: { startsWith: `bridge:${programName}:fee-burn:` } }, NOT: { key: { startsWith: `bridge:${programName}:release:` } } },
      orderBy: [{ createdAt: 'asc' }, { key: 'asc' }], take: config.maxRetryWithdrawalsPerCycle,
    })
    const resumed = await Promise.allSettled(pending.map(async row => {
      const event = ({ post: 'bridge_cblock_posted', attest: 'bridge_attested', handoff: 'bridge_handed_on', 'close-cblock': 'bridge_account_closed', 'close-set': 'bridge_account_closed' } as Record<string, string>)[row.key.split(':')[2]]
      try {
        if (await journal.follow(row.key) !== 'confirmed') throw new Error('Bridge transaction confirmation deadline exceeded.')
        await receipt(row, 'confirmed')
        log({ event, actionKey: row.key, signature: row.signature, commitment: 'confirmed', resumed: true })
      } catch (error) {
        log({ event: `${event}_failed`, actionKey: row.key, error: reason(error) })
        throw error
      }
    }))
    for (const result of resumed) if (result.status === 'rejected') throw result.reason
    // Keep only the accounts needed by recorded unpaid or not-yet-scanned withdrawals. A failed
    // release in an old epoch must not stop unrelated cblocks and validator sets returning rent.
    const unpaid = await db.bridgeUnpaidWithdrawal.findMany({ where: { program: programName }, select: { cblock: true, epoch: true } })
    const keptCblocks = new Set(unpaid.map(row => exact(row.cblock, 'unpaid cblock')))
    const keptEpochs = new Set(unpaid.map(row => exact(row.epoch, 'unpaid epoch')))
    const cblocks = await programAccounts<Cblock>('Cblock')
    for (const cblock of cblocks) {
      if (!cblock.address.equals(pda(Buffer.from('cblock'), Buffer.from(cblock.value.hash)))) throw new Error('Cblock PDA differs.')
      const slot = exact(cblock.value.slot, 'cblock slot')
      const pendingFeeBurn = nextFeeBurnEpoch !== null && Math.floor(slot * 4 / epochSlots) >= nextFeeBurnEpoch && (slot + 1) * 4 % epochSlots === 0
      if (pendingFeeBurn || keptCblocks.has(slot) || unscannedCblock !== null && slot >= unscannedCblock) continue
      if (cblock.value.payer.equals(signer.publicKey)) await closeCblock(cblock.address, slot)
    }
    sets = await programAccounts<ValidatorSet>('ValidatorSet', 'finalized')
    const newestFinalizedEpoch = sets.length ? Math.max(...sets.map(set => exact(set.value.epoch, 'validator epoch'))) : null
    for (const set of sets) {
      const epoch = exact(set.value.epoch, 'validator epoch')
      if (newestFinalizedEpoch === null || epoch + 1 >= newestFinalizedEpoch || epoch >= currentEpoch || nextFeeBurnEpoch !== null && epoch >= nextFeeBurnEpoch || keptEpochs.has(epoch) || unscannedEpoch !== null && epoch >= unscannedEpoch || !set.value.payer.equals(signer.publicKey)) continue
      await action('bridge_account_closed', `close-set:${epoch}`, { account: set.address.toBase58(), kind: 'validator_set', epoch }, async () => [await program.methods.closeValidatorSet().accountsStrict({ payer: signer.publicKey, set: set.address, newest: setPda(newestFinalizedEpoch) }).instruction()], async commitment => !await account<ValidatorSet>(solana, set.address, 'ValidatorSet', commitment))
    }
    await report(newestFinalizedEpoch === null ? {} : { lastHandedEpoch: BigInt(newestFinalizedEpoch) })
    if (burnFailure) throw burnFailure.error
  }
  return { runCycle }
}
