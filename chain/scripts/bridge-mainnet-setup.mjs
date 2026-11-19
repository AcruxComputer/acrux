// Road build 3 step 6, the bridge authority's one-time setup on Solana mainnet, once the bridge
// rule's slot has passed on Acrux Net: `initialize` (only the program's upgrade authority can),
// the light client seeded with the validator set of the newest finalized cblock's epoch, and
// ACRUX admitted as the one mint across. Each step is skipped when it is already done.
// Usage (from backend/chain):
//   node scripts/bridge-mainnet-setup.mjs --rpc <solana rpc url> --acrux https://rpc.acrux.computer \
//     --authority <upgrade authority keypair file> --mint 7Evw4uxTxLGGEFEvXLUNYyc1J7PnFk9EYwTUFagARUX
import { readFileSync } from 'node:fs'
import { Connection, Keypair, PublicKey, SystemProgram } from '@solana/web3.js'
import { DEFAULT_EPOCH_SLOTS, bridgeProgram, cblockProof, pdas, seed } from './bridge-relay.mjs'

const arg = name => {
  const i = process.argv.indexOf(`--${name}`)
  if (i < 0 || !process.argv[i + 1]) throw new Error(`--${name} is required`)
  return process.argv[i + 1]
}
const authority = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(arg('authority'), 'utf8'))))
const acrux = arg('acrux'), mint = new PublicKey(arg('mint'))
const program = bridgeProgram(new Connection(arg('rpc'), 'confirmed'), authority)

const head = await (await fetch(acrux, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getFinalizedCblock' }) })).json()
const proof = await cblockProof(acrux, head.result.consensusSlot)
if (!proof.chainId) throw new Error('The newest finalized cblock carries no chain tag: the bridge rule slot has not passed yet.')
await seed(program, proof, DEFAULT_EPOCH_SLOTS)

const { bridge } = pdas(program.programId)
const allowed = PublicKey.findProgramAddressSync([Buffer.from('allowed'), mint.toBuffer()], program.programId)[0]
if (!(await program.provider.connection.getAccountInfo(allowed))) {
  await program.methods.allowMint().accountsStrict({ authority: authority.publicKey, bridge, mint, allowed, systemProgram: SystemProgram.programId }).rpc()
}
const state = await program.account.bridge.fetch(bridge)
console.log(JSON.stringify({
  program: program.programId.toBase58(), bridge: bridge.toBase58(), authority: state.authority.toBase58(),
  epochSlots: state.epochSlots.toString(), seededFrom: proof.consensusSlot, seededEpoch: Math.floor(proof.consensusSlot * 4 / DEFAULT_EPOCH_SLOTS),
  validators: proof.validators.length, chainId: proof.chainId, allowedMint: mint.toBase58(), allowed: allowed.toBase58(),
}))
