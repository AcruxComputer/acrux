import { ACRUX_BRIDGE_IDL } from '@acrux/contract'
import { BorshAccountsCoder, Program, type Idl } from '@coral-xyz/anchor'
import BN from 'bn.js'
import type { PrismaClient } from '@prisma/client'
import { Connection, PublicKey, SystemProgram, SYSVAR_INSTRUCTIONS_PUBKEY, type AccountInfo, type Keypair, type TransactionInstruction } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, createAssociatedTokenAccountIdempotentInstruction, getAssociatedTokenAddressSync, unpackMint } from '@solana/spl-token'
import { createJournal } from '../engines/keeper/journal.js'
import { prismaKeeperStore } from '../engines/keeper/prisma-store.js'
import type { BridgeRelayerConfig } from './config.js'
import { attestInstruction, exact, outboxProof, readProof, u64, type Proof } from './proof.js'

/** An error's message for the log, with any URL (which may carry an RPC key) left out. */
export const reason = (error: unknown) => error instanceof Error ? `${error.name}: ${error.message.replace(/https?:\/\/\S+/g, '<url>')}` : 'Error'
const RPC_MS = 10_000
const CONFIRM_MS = 30_000
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))

/** HTTP-only connections: every request is aborted; confirmation uses the keeper journal's bounded poll. */
export function bridgeConnection(url: string) {
  return new Connection(url, { commitment: 'finalized', disableRetryOnRateLimit: true, fetch: (input, init) => fetch(input, {
    ...init, signal: init?.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(RPC_MS)]) : AbortSignal.timeout(RPC_MS),
  }) })
}

type ValidatorSet = { epoch: BN; validators: { key: PublicKey; stake: BN }[]; payer: PublicKey }
type Cblock = { slot: BN; epoch: BN; hash: number[]; certified: boolean; notar: number; finalize: number; payer: PublicKey }
type Withdrawal = { sequence: BN; recipient: PublicKey; solana_mint: PublicKey; amount: BN; slot: BN }

export function createBridgeRelayer(options: { config: BridgeRelayerConfig; signer: Keypair; db: PrismaClient; isLeader: () => boolean; log: (entry: Record<string, unknown>) => void }) {
  const { config, signer, db, isLeader, log } = options
  const solana = bridgeConnection(config.rpcUrl), acrux = bridgeConnection(config.gatewayUrl)
  const idl = { ...ACRUX_BRIDGE_IDL, address: config.program.toBase58() } as unknown as Idl
  const coder = new BorshAccountsCoder(idl), program = new Program(idl, { connection: solana })
  const store = prismaKeeperStore(db)
  const journal = createJournal({ connection: solana, signer, store, isLeader, followMs: CONFIRM_MS })
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
  async function account<T>(connection: Connection, address: PublicKey, name: string): Promise<T | null> {
    const info = await connection.getAccountInfo(address, 'finalized')
    return info ? decode<T>(name, info) : null
  }
  async function isReleased(sequence: number) {
    const state = await account<{ bucket: BN; bits: number[] }>(solana, releasedPda(sequence), 'Released')
    if (!state) return false
    if (exact(state.bucket, 'released bucket') !== Math.floor(sequence / 8192)) throw new Error('Released bucket differs.')
    const bit = sequence % 8192
    return !!(state.bits[Math.floor(bit / 8)] & (1 << (bit % 8)))
  }
  async function programAccounts<T>(name: string) {
    const accounts = await solana.getProgramAccounts(config.program, { commitment: 'finalized', filters: [{ memcmp: { offset: 0, bytes: coder.accountDiscriminator(name).toString('base64'), encoding: 'base64' } }] })
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
  async function finalized(signature: string) {
    const deadline = Date.now() + CONFIRM_MS
    while (Date.now() < deadline) {
      const status = (await solana.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
      if (status?.err) throw new Error('Bridge transaction failed on chain.')
      if (status?.confirmationStatus === 'finalized') return
      await pause(500)
    }
    throw new Error('Bridge transaction finalization deadline exceeded.')
  }
  async function action(event: string, key: string, fields: Record<string, unknown>, instructions: () => Promise<TransactionInstruction[]>) {
    const journalKey = `bridge:${programName}:${key}`
    try {
      own()
      // The caller's finalized account read proves this action is needed again (e.g. repost after close).
      const previous = await store.get(journalKey)
      if (previous?.state === 'confirmed') await store.update(journalKey, { state: 'expired' })
      if (await journal.execute(journalKey, instructions) !== 'confirmed') throw new Error('Bridge transaction confirmation deadline exceeded.')
      const row = await store.get(journalKey)
      if (!row) throw new Error('Bridge transaction journal is missing.')
      await finalized(row.signature)
      log({ event, ...fields, signature: row.signature })
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
      await action('bridge_cblock_posted', `post:${proof.hash}`, { cblock: proof.consensusSlot, epoch }, async () => [await program.methods.postCblock([...hash], [...Buffer.from(proof.outboxRoot, 'hex')], proof.preimage).accountsStrict({ payer: signer.publicKey, bridge, cblock, systemProgram: SystemProgram.programId }).instruction()])
      state = await account<Cblock>(solana, cblock, 'Cblock')
      if (!state) throw new Error('Posted bridge cblock is missing.')
    }
    if (!state.certified) {
      const set = await account<ValidatorSet>(solana, setPda(epoch), 'ValidatorSet')
      if (!set || set.validators.length !== proof.validators.length || set.validators.some((v, i) => !v.key.equals(proof.validators[i]) || v.stake.toString() !== proof.stakes[i])) throw new Error('Gateway validators differ from the certified validator set.')
      // Do not pay for signatures already counted, and never mix Finalize and Notar in one batch.
      for (const kind of ['finalize', 'notar'] as const) {
        const votes = proof.votes.filter((vote, i, all) => vote.kind === kind && !(Number(state![kind]) >>> vote.validator & 1) && all.findIndex(v => v.kind === vote.kind && v.validator === vote.validator) === i)
        for (let i = 0; i < votes.length && !state.certified; i += 4) {
          const batch = votes.slice(i, i + 4)
          await action('bridge_attested', `attest:${proof.hash}:${kind}:${batch.map(v => v.validator).join(',')}`, { cblock: proof.consensusSlot, epoch, kind, signatures: batch.length }, async () => [attestInstruction(proof, batch), await program.methods.attestCblock().accountsStrict({ bridge, cblock, set: setPda(epoch), instructions: SYSVAR_INSTRUCTIONS_PUBKEY }).instruction()])
          state = await account<Cblock>(solana, cblock, 'Cblock')
          if (!state) throw new Error('Attested bridge cblock is missing.')
        }
        if (state.certified) break
      }
      if (!state.certified) throw new Error('Gateway proof does not carry enough stake to certify its cblock.')
    }
    await report({ lastCblock: BigInt(proof.consensusSlot) })
    return cblock
  }
  async function relay(proof: Proof, handoffEpoch?: number) {
    const cblock = await certify(proof)
    for (const [index, entry] of proof.entries.entries()) {
      if (entry.kind === 'set') {
        if (entry.epoch !== handoffEpoch || await account<ValidatorSet>(solana, setPda(entry.epoch), 'ValidatorSet')) continue
        await action('bridge_handed_on', `handoff:${entry.epoch}`, { epoch: entry.epoch, cblock: proof.consensusSlot }, async () => [await program.methods.handOff(new BN(entry.epoch), entry.validators, outboxProof(proof.outbox, index)).accountsStrict({ payer: signer.publicKey, cblock, next: setPda(entry.epoch), systemProgram: SystemProgram.programId }).instruction()])
        await report({ lastHandedEpoch: BigInt(entry.epoch) })
      } else {
        if (await isReleased(entry.sequence)) continue
        const mintInfo = await solana.getAccountInfo(entry.mint, 'finalized')
        if (!mintInfo || ![TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID].some(id => id.equals(mintInfo.owner))) throw new Error('Withdrawal mint is not an SPL token mint.')
        unpackMint(entry.mint, mintInfo, mintInfo.owner)
        const tokenProgram = mintInfo.owner
        const destination = getAssociatedTokenAddressSync(entry.mint, entry.recipient, true, tokenProgram)
        await action('bridge_released', `release:${entry.sequence}`, { sequence: entry.sequence, cblock: proof.consensusSlot, amount: entry.amount.toString() }, async () => [
          createAssociatedTokenAccountIdempotentInstruction(signer.publicKey, destination, entry.recipient, entry.mint, tokenProgram),
          await program.methods.release(new BN(entry.sequence), entry.recipient, entry.amount, outboxProof(proof.outbox, index)).accountsStrict({ payer: signer.publicKey, cblock, mint: entry.mint, vault: pda(Buffer.from('vault'), entry.mint.toBuffer()), destination, released: releasedPda(entry.sequence), tokenProgram, systemProgram: SystemProgram.programId }).instruction(),
        ])
      }
    }
    const state = await account<Cblock>(solana, cblock, 'Cblock')
    if (state?.payer.equals(signer.publicKey)) await closeCblock(cblock, proof.consensusSlot)
  }
  async function closeCblock(cblock: PublicKey, slot: number) {
    await action('bridge_account_closed', `close-cblock:${cblock.toBase58()}`, { account: cblock.toBase58(), kind: 'cblock', cblock: slot }, async () => [await program.methods.closeCblock().accountsStrict({ payer: signer.publicKey, cblock }).instruction()])
  }
  async function runCycle() {
    own()
    // Reconcile this task's signed bytes before account guards can skip an action that landed
    // during a restart. Never resume the buyback journal under the bridge's separate lease.
    const pending = await db.keeperTransaction.findMany({ where: { state: 'signed', key: { startsWith: `bridge:${programName}:` } }, orderBy: { createdAt: 'asc' } })
    for (const row of pending) {
      const event = ({ post: 'bridge_cblock_posted', attest: 'bridge_attested', handoff: 'bridge_handed_on', release: 'bridge_released', 'close-cblock': 'bridge_account_closed', 'close-set': 'bridge_account_closed' } as Record<string, string>)[row.key.split(':')[2]]
      try {
        if (await journal.follow(row.key) !== 'confirmed') throw new Error('Bridge transaction confirmation deadline exceeded.')
        await finalized(row.signature)
        log({ event, actionKey: row.key, signature: row.signature, resumed: true })
      } catch (error) {
        log({ event: `${event}_failed`, actionKey: row.key, error: reason(error) })
        throw error
      }
      await report({ lastRelayedAt: new Date() })
    }
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
    let newestEpoch = Math.max(...sets.map(set => exact(set.value.epoch, 'validator epoch')))
    while (newestEpoch < currentEpoch) {
      await relay(await proof({ epoch: newestEpoch }), newestEpoch + 1)
      newestEpoch++
    }
    // Read the counter after the finalized frontier. Accounts are monotonic on Acrux; records
    // newer than that frontier are left for the next tick and forbid old-set cleanup this tick.
    const outbound = await account<{ withdrawals: BN }>(acrux, pda(Buffer.from('outbound')), 'Outbound')
    const count = outbound ? exact(outbound.withdrawals, 'withdrawal count') : 0
    const row = await db.bridgeRelayerState.findUniqueOrThrow({ where: { program: programName } })
    let next = exact(row.nextWithdrawalSequence, 'withdrawal cursor')
    while (next < count) {
      const withdrawal = await account<Withdrawal>(acrux, pda(Buffer.from('withdrawal'), u64(next)), 'Withdrawal')
      if (!withdrawal || exact(withdrawal.sequence, 'withdrawal sequence') !== next) throw new Error('Acrux withdrawal record is missing or inconsistent.')
      if (exact(withdrawal.slot, 'withdrawal slot') > finalizedSlot) return
      if (!await isReleased(next)) {
        const cblock = Math.floor(exact(withdrawal.slot, 'withdrawal slot') / 4), burnProof = await proof({ consensusSlot: cblock })
        if (!burnProof.entries.some(entry => entry.kind === 'withdrawal' && entry.sequence === next && entry.mint.equals(withdrawal.solana_mint) && entry.recipient.equals(withdrawal.recipient) && entry.amount.eq(withdrawal.amount))) throw new Error('Gateway outbox does not match the burned withdrawal.')
        await relay(burnProof)
      }
      next++
      await report({ nextWithdrawalSequence: BigInt(next) })
    }
    // Only a complete successful withdrawal scan permits cleanup. Old epochs cannot produce
    // more withdrawals after this finalized frontier, so no future proof needs their sets.
    const cblocks = await programAccounts<Cblock>('Cblock')
    for (const cblock of cblocks) {
      if (!cblock.address.equals(pda(Buffer.from('cblock'), Buffer.from(cblock.value.hash)))) throw new Error('Cblock PDA differs.')
      if (cblock.value.payer.equals(signer.publicKey)) await closeCblock(cblock.address, exact(cblock.value.slot, 'cblock slot'))
    }
    sets = await programAccounts<ValidatorSet>('ValidatorSet')
    for (const set of sets) {
      const epoch = exact(set.value.epoch, 'validator epoch')
      if (epoch + 1 >= newestEpoch || epoch >= currentEpoch || !set.value.payer.equals(signer.publicKey)) continue
      await action('bridge_account_closed', `close-set:${epoch}`, { account: set.address.toBase58(), kind: 'validator_set', epoch }, async () => [await program.methods.closeValidatorSet().accountsStrict({ payer: signer.publicKey, set: set.address, newest: setPda(newestEpoch) }).instruction()])
    }
    await report({ lastHandedEpoch: BigInt(newestEpoch) })
  }
  return { runCycle }
}