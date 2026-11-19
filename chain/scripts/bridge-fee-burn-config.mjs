// One authority setup transaction after the Solana bridge upgrade; no deploy or Acrux placement.
// Usage: node scripts/bridge-fee-burn-config.mjs --rpc <Solana RPC> --authority <keypair.json> --mint <native Solana mint>
import { readFileSync } from 'node:fs'
import { Connection, Keypair, PublicKey, SystemProgram, Transaction } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID } from '@solana/spl-token'
import { bridgeProgram, pdas } from './bridge-relay.mjs'

const arg = name => {
  const at = process.argv.indexOf(`--${name}`)
  if (at < 0 || !process.argv[at + 1]) throw new Error(`--${name} is required`)
  return process.argv[at + 1]
}
try {
  const secret = JSON.parse(readFileSync(arg('authority'), 'utf8'))
  if (!Array.isArray(secret) || secret.length !== 64 || !secret.every(value => Number.isInteger(value) && value >= 0 && value <= 255)) throw new Error('The authority file must be a Solana keypair JSON array.')
  const authority = Keypair.fromSecretKey(Uint8Array.from(secret)), mint = new PublicKey(arg('mint'))
  const connection = new Connection(arg('rpc'), { commitment: 'finalized', disableRetryOnRateLimit: true,
    fetch: (input, init) => fetch(input, { ...init, signal: init?.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(10_000)]) : AbortSignal.timeout(10_000) }),
  })
  const program = bridgeProgram(connection, authority), { bridge, feeBurnConfig: config } = pdas(program.programId)
  const bridgeState = await program.account.bridge.fetch(bridge, 'finalized')
  if (!bridgeState.authority.equals(authority.publicKey)) throw new Error('This keypair is not the bridge authority.')
  const existing = await connection.getAccountInfo(config, 'finalized')
  let signature = null
  if (existing && !(existing.owner.equals(SystemProgram.programId) && !existing.executable && existing.data.length === 0)) {
    if (!existing.owner.equals(program.programId) || existing.executable) throw new Error('The fee burn configuration has an invalid owner.')
    const state = await program.account.feeBurnConfig.fetch(config, 'finalized')
    if (!state.mint.equals(mint)) throw new Error('The immutable fee burn configuration names another mint.')
  } else {
    const mintInfo = await connection.getAccountInfo(mint, 'finalized')
    if (!mintInfo || ![TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID].some(owner => owner.equals(mintInfo.owner))) throw new Error('The native mint must belong to an SPL Token program.')
    const instruction = await program.methods.initFeeBurnConfig().accountsStrict({ authority: authority.publicKey, bridge, mint, config, tokenProgram: mintInfo.owner, systemProgram: SystemProgram.programId }).instruction()
    const latest = await connection.getLatestBlockhash('finalized')
    const transaction = new Transaction({ ...latest, feePayer: authority.publicKey }).add(instruction)
    transaction.sign(authority)
    signature = await connection.sendRawTransaction(transaction.serialize(), { preflightCommitment: 'finalized', maxRetries: 0 })
    let done = false
    const deadline = Date.now() + 60_000
    while (Date.now() < deadline) {
      const state = (await connection.getSignatureStatuses([signature], { searchTransactionHistory: true })).value[0]
      if (state?.err) throw new Error(`The configuration transaction failed: ${JSON.stringify(state.err)}`)
      if (state?.confirmationStatus === 'finalized') { done = true; break }
      await new Promise(resolve => setTimeout(resolve, 500))
    }
    if (!done) throw new Error('Fee burn configuration confirmation deadline exceeded; rerun to reconcile the on-chain account.')
    const state = await program.account.feeBurnConfig.fetch(config, 'finalized')
    if (!state.mint.equals(mint)) throw new Error('The finalized fee burn configuration names another mint.')
  }
  console.log(JSON.stringify({ event: 'bridge_fee_burn_configured', program: program.programId.toBase58(), mint: mint.toBase58(), config: config.toBase58(), signature }))
} catch (error) {
  console.error(error instanceof Error ? error.message.replace(/https?:\/\/\S+/g, '<url>') : 'Fee burn configuration failed.')
  process.exitCode = 1
}
