// Road build 1, step 7: programs placed at their mainnet addresses from a named slot, each driven by
// its own SDK with only the RPC URL changed. @solana/spl-memo writes a memo through Memo4c2p, the
// memo program it targets; @raydium-io/raydium-sdk-v2 creates a Raydium CPMM pool for two fresh
// tokens on the CPMM's first fee tier and swaps through it.
// Usage: node test/step7-placements.mjs [rpcUrl] [faucetUrl] [funded keypair file, used instead of the faucet]
import { readFileSync } from 'node:fs'
import { Connection, Keypair, PublicKey, SYSVAR_CLOCK_PUBKEY, Transaction, sendAndConfirmTransaction } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID, createMint, getAccount, getOrCreateAssociatedTokenAccount, mintTo } from '@solana/spl-token'
import { MEMO_PROGRAM_ID, createMemoInstruction } from '@solana/spl-memo'
import {
  CREATE_CPMM_POOL_FEE_ACC, CREATE_CPMM_POOL_PROGRAM, CpmmConfigInfoLayout, CurveCalculator, Raydium, TxVersion, getCpmmPdaAmmConfigId,
} from '@raydium-io/raydium-sdk-v2'
import BN from 'bn.js'

const [rpc = 'https://rpc.acrux.computer', faucet = 'https://api.acrux.computer/v1/faucet', funded] = process.argv.slice(2)
const connection = new Connection(rpc, 'confirmed')
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const payer = funded ? Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(funded, 'utf8')))) : Keypair.generate()
const show = (program, what, signature = '') => console.log(`${program.padEnd(14)} ${what.padEnd(52)} ${signature}`)
// Every step has a deadline: a step that does not finish in time stops the run and names itself.
const within = (seconds, what, promise) => Promise.race([
  promise,
  new Promise((_, reject) => setTimeout(() => reject(new Error(`${what}: no answer in ${seconds} s`)), seconds * 1000).unref()),
])
console.log(`payer ${payer.publicKey.toBase58()} on ${rpc}`)

if (!funded) {
  const body = faucet.includes('internal') ? { address: payer.publicKey.toBase58(), amountLamports: '2000000000' } : { address: payer.publicKey.toBase58() }
  const grant = await within(30, 'faucet', fetch(faucet, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }))
  if (!grant.ok) throw new Error(`faucet refused: ${await grant.text()}`)
  console.log('faucet granted')
}
let lamports = 0
for (const started = Date.now(); lamports === 0; await sleep(500)) {
  if (Date.now() - started > 30_000) throw new Error('the payer holds 0 lamports after 30 s: the grant did not land, or the key is not funded')
  lamports = await within(15, 'getBalance', connection.getBalance(payer.publicKey))
}
console.log(`payer holds ${lamports / 1e9} ACRUX\n`)

// Memo4c2p through @solana/spl-memo.
const memo = await within(60, 'memo', sendAndConfirmTransaction(connection, new Transaction().add(createMemoInstruction('gm from Acrux', [payer.publicKey])), [payer]))
const memoTx = await within(15, 'memo lookup', connection.getTransaction(memo, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 }))
show('Memo', `${MEMO_PROGRAM_ID.toBase58().slice(0, 8)}… ${memoTx.meta.logMessages.find(l => l.includes('Memo (len'))?.replace('Program log: ', '') ?? 'executed'}`, memo)

// Raydium CPMM through @raydium-io/raydium-sdk-v2: two tokens, a pool on the first fee tier, a swap.
const mints = []
for (const name of ['A', 'B']) {
  const mint = await within(60, `token ${name}`, createMint(connection, payer, payer.publicKey, null, 6, Keypair.generate(), { commitment: 'confirmed' }, TOKEN_PROGRAM_ID))
  const account = await within(60, `token ${name} account`, getOrCreateAssociatedTokenAccount(connection, payer, mint, payer.publicKey))
  await within(60, `token ${name} mint`, mintTo(connection, payer, mint, account.address, payer, 1_000_000_000_000))
  mints.push({ address: mint.toBase58(), decimals: 6, programId: TOKEN_PROGRAM_ID.toBase58(), account: account.address })
  show('SPL Token', `token ${name} ${mint.toBase58()}, 1,000,000 minted`)
}
const raydium = await within(60, 'Raydium.load', Raydium.load({ connection, owner: payer, cluster: 'mainnet', disableFeatureCheck: true, disableLoadToken: true, blockhashCommitment: 'confirmed' }))
const configId = getCpmmPdaAmmConfigId(CREATE_CPMM_POOL_PROGRAM, 0).publicKey
const config = CpmmConfigInfoLayout.decode((await within(15, 'fee tier', connection.getAccountInfo(configId))).data)
const feeConfig = {
  id: configId.toBase58(), index: config.index, protocolFeeRate: config.protocolFeeRate.toNumber(), tradeFeeRate: config.tradeFeeRate.toNumber(),
  fundFeeRate: config.fundFeeRate.toNumber(), createPoolFee: config.createPoolFee.toString(), creatorFeeRate: config.creatorFeeRate?.toNumber?.() ?? 0,
}
const created = await raydium.cpmm.createPool({
  programId: CREATE_CPMM_POOL_PROGRAM, poolFeeAccount: CREATE_CPMM_POOL_FEE_ACC, mintA: mints[0], mintB: mints[1],
  mintAAmount: new BN(100_000_000_000), mintBAmount: new BN(400_000_000_000), startTime: new BN(0), feeConfig,
  associatedOnly: false, ownerInfo: { useSOLBalance: true }, txVersion: TxVersion.LEGACY,
})
const { txId: createTx } = await within(90, 'create pool', created.execute({ sendAndConfirm: true }))
const poolId = created.extInfo.address.poolId.toBase58()
show('Raydium CPMM', `pool ${poolId}, 100,000 A + 400,000 B, fee tier ${config.tradeFeeRate.toNumber() / 1e4}%`, createTx)

const { poolInfo, poolKeys, rpcData } = await within(30, 'pool read', raydium.cpmm.getPoolInfoFromRpc(poolId))
// The CPMM opens a new pool one second after the clock it was created at; a swap before that fails
// NotApproved, as on mainnet. Wait for the chain's own clock to pass it.
const clock = async () => Number((await within(15, 'clock', connection.getAccountInfo(SYSVAR_CLOCK_PUBKEY))).data.readBigInt64LE(32))
for (const started = Date.now(); (await clock()) < rpcData.openTime.toNumber(); await sleep(250)) {
  if (Date.now() - started > 30_000) throw new Error('the pool did not open within 30 s of the chain clock')
}
const inputAmount = new BN(1_000_000_000)
const baseIn = poolInfo.mintA.address === mints[0].address
const swapResult = CurveCalculator.swapBaseInput(inputAmount, baseIn ? rpcData.baseReserve : rpcData.quoteReserve, baseIn ? rpcData.quoteReserve : rpcData.baseReserve, rpcData.configInfo.tradeFeeRate, rpcData.configInfo.creatorFeeRate, rpcData.configInfo.protocolFeeRate, rpcData.configInfo.fundFeeRate, rpcData.feeOn === 0 || (rpcData.feeOn === 1 && baseIn) || (rpcData.feeOn === 2 && !baseIn))
const before = (await within(15, 'balance B', getAccount(connection, mints[1].account))).amount
const swap = await raydium.cpmm.swap({ poolInfo, poolKeys, inputAmount, swapResult, slippage: 0.01, baseIn, txVersion: TxVersion.LEGACY })
const { txId: swapTx } = await within(90, 'swap', swap.execute({ sendAndConfirm: true }))
const got = (await within(15, 'balance B', getAccount(connection, mints[1].account))).amount - before
show('Raydium CPMM', `swap 1,000 A for ${(Number(got) / 1e6).toFixed(6)} B (quoted ${(Number(swapResult.outputAmount ?? swapResult.destinationAmountSwapped) / 1e6).toFixed(6)})`, swapTx)
if (got <= 0n) throw new Error('the swap paid out nothing')
console.log('\nMemo4c2p and Raydium CPMM ran at their mainnet addresses, each through its own SDK')
process.exit(0)
