// Sets up a local bridge stand once its validator and Acrux net are up: seeds the light client from
// the net's first proof, makes a 6-decimal test mint and admits it, and funds two local keypairs: a
// user's (10 SOL and 1,000,000 test tokens) and the relayer's fee wallet (2 SOL). Writes
// `user-keypair.json`, `relayer-keypair.json` and `stand.json` into <out dir>.
// Usage (from backend/chain): node scripts/bridge-stand.mjs <out dir> [epoch slots, as in the rule]
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import anchor from '@coral-xyz/anchor'
import { TOKEN_PROGRAM_ID, createMint, getOrCreateAssociatedTokenAccount, mintTo } from '@solana/spl-token'
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { devKeypair } from './dev-keys.mjs'
import { bridgeProgram, cblockProof, pdas, seed } from './bridge-relay.mjs'

const [out, epochSlots = '1536'] = process.argv.slice(2)
const connection = new Connection('http://127.0.0.1:4990', 'confirmed')
const authority = devKeypair('local-authority')
const program = bridgeProgram(connection, authority)
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

let first
for (let t = 0; t < 120 && !first; t++) {
  for (let c = 1; c < 9 && !first; c++) first = await cblockProof('http://127.0.0.1:4930', c).catch(() => null)
  if (!first) await sleep(500)
}
if (!first) throw new Error('No cblock proof from the net after 60 s')
await seed(program, first, Number(epochSlots))

const mint = await createMint(connection, authority, authority.publicKey, null, 6, Keypair.generate(), { commitment: 'confirmed' }, TOKEN_PROGRAM_ID)
const { bridge } = pdas(program.programId)
const allowed = PublicKey.findProgramAddressSync([Buffer.from('allowed'), mint.toBuffer()], program.programId)[0]
await program.methods.allowMint().accountsStrict({ authority: authority.publicKey, bridge, mint, allowed, systemProgram: SystemProgram.programId }).rpc()

const user = Keypair.generate(), relayer = Keypair.generate()
await sendAndConfirmTransaction(connection, new Transaction().add(
  SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: user.publicKey, lamports: 10 * LAMPORTS_PER_SOL }),
  SystemProgram.transfer({ fromPubkey: authority.publicKey, toPubkey: relayer.publicKey, lamports: 2 * LAMPORTS_PER_SOL }),
), [authority])
const account = await getOrCreateAssociatedTokenAccount(connection, authority, mint, user.publicKey)
await mintTo(connection, authority, mint, account.address, authority, 1_000_000_000_000n)

writeFileSync(join(out, 'user-keypair.json'), JSON.stringify([...user.secretKey]))
writeFileSync(join(out, 'relayer-keypair.json'), JSON.stringify([...relayer.secretKey]))
const stand = { program: program.programId.toBase58(), mint: mint.toBase58(), user: user.publicKey.toBase58(), relayer: relayer.publicKey.toBase58(), seededEpoch: Math.floor(first.consensusSlot * 4 / Number(epochSlots)) }
writeFileSync(join(out, 'stand.json'), JSON.stringify(stand, null, 1))
console.log(JSON.stringify(stand))
