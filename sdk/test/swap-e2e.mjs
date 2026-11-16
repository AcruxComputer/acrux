// End to end on a running net: faucet grant (SOL and demo tokens), quote, one demo-pool swap through the SDK.
// Usage: node test/swap-e2e.mjs <rpc-url> [node private faucet url]
import { Keypair, PublicKey, Transaction, TransactionInstruction, SystemProgram } from '@solana/web3.js'
import { connect } from '../dist/index.js'
const rpcUrl = process.argv[2] ?? 'http://127.0.0.1:4930'
const rpc = async (method, params = {}) => { const r = await (await fetch(rpcUrl, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) })).json(); if (r.error) throw new Error(JSON.stringify(r.error)); return r.result }
const TOKEN = new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'), ATA = new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL')
const PROGRAM = new PublicKey('CUBR9LNdTwxwp5ARBQj1EopfuH8dr8u4i7DsXcb8HbHN')
const ata = (owner, mint) => PublicKey.findProgramAddressSync([owner.toBuffer(), TOKEN.toBuffer(), mint.toBuffer()], ATA)[0]
const user = Keypair.generate()
const pool = await rpc('getDemoPool'); console.log('pool', JSON.stringify(pool))
const faucetUrl = process.argv[3] ?? 'http://127.0.0.1:4989/internal/faucet'
const grant = await (await fetch(faucetUrl, { method: 'POST', body: JSON.stringify({ address: user.publicKey.toBase58(), amountLamports: '2000000000' }) })).json()
console.log('grant', grant.transactionId ?? JSON.stringify(grant))
const acrux = connect(rpcUrl)
for await (const u of acrux.tx.follow(grant.transactionId)) if (['finalized', 'failed', 'expired'].includes(u.state)) { console.log('grant', u.state, u.transaction?.result, (u.transaction?.logs ?? []).slice(-2)); break }
const amountIn = 50_000_000n // 50 AXA of the 100 the faucet grants
const quote = await rpc('getQuote', { inputMint: pool.mints[0].address, amountIn: amountIn.toString(), slippageBps: 100 }); console.log('quote', JSON.stringify(quote))
const [mintA, mintB] = pool.mints.map(m => new PublicKey(m.address))
const vaultA = PublicKey.findProgramAddressSync([Buffer.from('vault'), new PublicKey(pool.address).toBuffer(), mintA.toBuffer()], PROGRAM)[0]
const vaultB = PublicKey.findProgramAddressSync([Buffer.from('vault'), new PublicKey(pool.address).toBuffer(), mintB.toBuffer()], PROGRAM)[0]
const data = Buffer.alloc(25); Buffer.from('f8c69e91e17587c8', 'hex').copy(data, 0); data.writeBigUInt64LE(amountIn, 8); data.writeBigUInt64LE(BigInt(quote.minimumOut), 16); data[24] = 1
const keys = [[user.publicKey, true, true], [new PublicKey(pool.address), false, false], [mintA, false, false], [mintB, false, false], [vaultA, false, true], [vaultB, false, true], [ata(user.publicKey, mintA), false, true], [ata(user.publicKey, mintB), false, true], [TOKEN, false, false], [ATA, false, false], [SystemProgram.programId, false, false]].map(([pubkey, isSigner, isWritable]) => ({ pubkey, isSigner, isWritable }))
const tx = new Transaction({ feePayer: user.publicKey, recentBlockhash: (await acrux.latestBlockhash()).blockhash }).add(new TransactionInstruction({ programId: PROGRAM, keys, data }))
tx.sign(user)
const { id } = await acrux.send(tx, { n: 3, t: 2, signer: user })
console.log('swap sent', id)
for await (const u of acrux.tx.follow(id)) { console.log('  ', u.state); if (['finalized', 'failed', 'expired'].includes(u.state)) { console.log('swap', u.transaction?.result, (u.transaction?.logs ?? []).filter(l => /Program log|failed|error/i.test(l)).slice(-4)); break } }
console.log('pool after', JSON.stringify((await rpc('getDemoPool')).reserves))
process.exit(0)
