import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import {
  Connection, Keypair, PublicKey, SystemProgram, SYSVAR_CLOCK_PUBKEY, VersionedTransaction,
} from '@solana/web3.js'
import {
  createAssociatedTokenAccountIdempotentInstruction,
  createCloseAccountInstruction, createSyncNativeInstruction,
  getAccount, getMint, NATIVE_MINT, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, unpackAccount,
} from '@solana/spl-token'
import { createBuybackService } from '../dist/buyback/service.js'
import { readBuybackConfig, loadLaunchKeypair } from '../dist/buyback/config.js'
import { createKeeperRuntime } from '../dist/engines/keeper/runtime.js'
import { buildUnsignedV0 } from '../dist/engines/tx/server/build.js'
import { sendSigned } from '../dist/engines/tx/server/send.js'
import { signatureOf } from '../dist/engines/tx/server/ed25519.js'
import { loadLookupTables } from '../dist/engines/tx/server/lookup-tables.js'
import { decodeTransaction } from '../dist/engines/indexer/decoder.js'
import { connectionRpc, readTransaction } from '../dist/engines/indexer/rpc.js'
import { descendants } from '../dist/engines/indexer/token-flows.js'
import * as addresses from '../dist/engines/launchpad/launchlab/addresses.js'
import { curveState, curveFeeRates, readGlobalConfig, readPlatformConfig, readPoolState } from '../dist/engines/launchpad/launchlab/accounts.js'
import { initializeWithToken2022Instruction, launchedPool } from '../dist/engines/launchpad/launchlab/launch.js'
import { createPlatformConfigInstruction } from '../dist/engines/launchpad/launchlab/platform.js'
import { tradeInstructions } from '../dist/engines/launchpad/launchlab/trade.js'
import { migrateToCpswapInstruction, migratedPoolFromTransaction } from '../dist/engines/launchpad/launchlab/migration.js'
import { decodeCpmmPool, decodeAmmConfig, cpmmRates, cpmmSwapInstruction, swapPaysCreatorFeeOnInput } from '../dist/engines/launchpad/launchlab/cpmm.js'
import { readFeeSnapshot } from '../dist/engines/launchpad/launchlab/fee-snapshot.js'
import { buyExactIn, completionQuote, launchState } from '../dist/engines/launchpad/curve/launchlab.js'
import { cpmmReserves, swapBaseInput } from '../dist/engines/launchpad/curve/cpmm.js'
import { ceilDiv, minimumOut } from '../dist/engines/launchpad/curve/math.js'
import { defaultPrepDirectory } from './buyback-clone-list.mjs'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
try { process.loadEnvFile(path.join(root, '.env')) } catch (error) { if (error.code !== 'ENOENT') throw error }

const pause = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds))

async function withApi(mint, wallet, check) {
  // Use the real API route without starting unrelated registry workers or the Acrux node.
  process.env.MODE = 'fixture'
  process.env.ACRUX_MINT = mint
  process.env.LAUNCH_WALLET = wallet
  const { env } = await import('../dist/config.js')
  env.MODE = 'fixture'
  env.ACRUX_MINT = mint
  env.LAUNCH_WALLET = wallet
  const { createFixtureApi } = await import('../dist/app.js')
  const { serve } = await import('../dist/engines/server/serve.js')
  const api = createFixtureApi()
  const server = serve(api.app, { host: '127.0.0.1', port: 5308, signals: false, close: api.close })
  try {
    if (!server.server.listening) await new Promise(resolve => server.server.once('listening', resolve))
    const response = await fetch('http://127.0.0.1:5308/v1/buyback')
    assert.equal(response.status, 200)
    await check(await response.json())
  } finally { await server.shutdown() }
}

export async function checkWaiting() {
  const wallet = Keypair.generate()
  let calls = 0
  const connection = new Connection('http://127.0.0.1:5340', {
    fetch: async () => { calls++; throw new Error('Blank mint made an RPC call') },
  })
  const config = readBuybackConfig({ ACRUX_MINT: '', LAUNCH_WALLET: wallet.publicKey.toBase58() })
  const service = createBuybackService({ config, connection, signer: wallet, db: {} })
  await service.runCycle()
  const runtime = createKeeperRuntime({
    lease: { acquire: async () => true, renew: async () => true, release: async () => {} },
    leaseMs: 1000, tickMs: 10, gate: async () => config.mint ? 'ready' : 'waiting', tick: service.runCycle,
  })
  const running = runtime.run()
  try {
    await pause(25)
    assert.equal(runtime.health.status, 'waiting')
    await withApi('', wallet.publicKey.toBase58(), body => {
      assert.equal(body.status, 'waiting')
      assert.equal(body.mint, null)
      assert.deepEqual(body.rows, [])
    })
    assert.equal(calls, 0, 'Waiting must make zero chain calls')
  } finally { await runtime.stop(); await running }
  console.log('PASS: blank mint is waiting with zero RPC calls')
}

async function send(connection, instructions, signers, tables = []) {
  const build = await buildUnsignedV0({ connection, payer: signers[0].publicKey, instructions, lookupTables: tables, cosigners: signers.slice(1).map(signer => signer.publicKey), walletSigned: false })
  build.transaction.sign(signers)
  return (await sendSigned(connection, { bytes: build.transaction.serialize(), blockhash: build.blockhash, lastValidBlockHeight: build.lastValidBlockHeight, contextSlot: build.contextSlot })).signature
}

async function fund(connection, wallet, lamports) {
  const signature = await connection.requestAirdrop(wallet, lamports)
  const result = await connection.confirmTransaction(signature, 'confirmed')
  assert.equal(result.value.err, null)
}

function localnetUrl() {
  if (!process.env.LOCALNET_RPC_URL) throw new Error('Set LOCALNET_RPC_URL to the clone validator (for example http://127.0.0.1:5340).')
  const url = new URL(process.env.LOCALNET_RPC_URL)
  if (!['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) throw new Error('This funded rehearsal only runs on a loopback LOCALNET_RPC_URL.')
  return url.href
}

async function runLocalnet() {
  const connection = new Connection(localnetUrl(), 'confirmed')
  const prepArg = process.argv.indexOf('--clone-dir')
  if (prepArg !== -1 && !process.argv[prepArg + 1]) throw new Error('--clone-dir needs the clone preparation directory')
  const prep = path.resolve(prepArg === -1 ? defaultPrepDirectory : process.argv[prepArg + 1])
  const relative = path.relative(path.resolve(root, '..'), prep)
  assert.ok(relative && !relative.startsWith('..') && !path.isAbsolute(relative), 'Clone directory must be inside backend')
  const metadata = JSON.parse(await readFile(path.join(prep, 'metadata.json'), 'utf8'))
  assert.equal(metadata.crankFile, 'migration-crank.json')
  const crank = Keypair.fromSecretKey(Uint8Array.from(JSON.parse(await readFile(path.join(prep, metadata.crankFile), 'utf8'))))
  const globalKey = new PublicKey(metadata.globalConfig), ammConfig = new PublicKey(metadata.ammConfig)
  assert.ok(globalKey.equals(addresses.globalConfigAddress(NATIVE_MINT)))
  const global = await readGlobalConfig(connection, globalKey)
  assert.ok(global.migrateToCpswapWallet.equals(crank.publicKey), 'Run the validator using this preparation directory')
  const tables = await loadLookupTables(connection, [addresses.RAYDIUM.lookupTable])
  assert.equal(tables[0].state.lastExtendedSlot, 0, 'Use the patched Raydium lookup table fixture')
  const launch = Keypair.generate(), buyer = Keypair.generate(), mint = Keypair.generate()
  const folder = await mkdtemp(path.join(root, '.buyback-e2e-'))
  let database
  try {
    const keypairFile = path.join(folder, 'launch-keypair.json')
    await writeFile(keypairFile, JSON.stringify(Array.from(launch.secretKey)), { mode: 0o600 })
    const config = readBuybackConfig({
      ACRUX_MINT: mint.publicKey.toBase58(), LAUNCH_WALLET: launch.publicKey.toBase58(), BUYBACK_LAUNCHPAD: 'launchlab',
      LAUNCH_KEYPAIR_FILE: keypairFile, SOLANA_RPC_URL: localnetUrl(), BUYBACK_DRY_RUN: 'false',
    })
    const signer = await loadLaunchKeypair(config)
    await assert.rejects(loadLaunchKeypair({ ...config, wallet: buyer.publicKey }), /does not match/)
    await fund(connection, launch.publicKey, 4_000_000_000)
    await fund(connection, buyer.publicKey, 1_000_000_000_000)
    await send(connection, [createPlatformConfigInstruction({
      admin: launch.publicKey, feeWallet: launch.publicKey, nftWallet: launch.publicKey, cpswapConfig: ammConfig,
      migrateNftInfo: { platformScale: 0n, creatorScale: 0n, burnScale: 1_000_000n },
      feeRate: 0n, creatorFeeRate: 5_000n, name: 'Keeper rehearsal', web: '', img: '',
    })], [launch], tables)
    const platformConfig = addresses.platformConfigAddress(launch.publicKey)
    const platform = await readPlatformConfig(connection, platformConfig)
    const curve = { supply: 1_000_000_000_000_000n, totalBaseSell: 793_100_000_000_000n, totalQuoteFundRaising: global.minQuoteFundRaising > 85_000_000_000n ? global.minQuoteFundRaising : 85_000_000_000n }
    const launchInput = {
      payer: launch.publicKey, creator: launch.publicKey, baseMint: mint.publicKey, globalConfig: globalKey,
      quoteMint: NATIVE_MINT, platformConfig, restrictions: platform,
      token: { name: 'Acrux keeper rehearsal', symbol: 'AKPR', uri: 'https://example.invalid/acrux-keeper.json' },
      curve, ammCreatorFeeOn: 'bothToken',
    }
    const rates = curveFeeRates(global, platform)
    const firstBuy = 100_000_000n
    const firstQuote = buyExactIn(launchState({ ...curve, migrateFee: global.migrateFee }), firstBuy, rates)
    await send(connection, [initializeWithToken2022Instruction(launchInput), ...tradeInstructions({
      payer: launch.publicKey, pool: launchedPool(launchInput, { token2022: true }), side: 'buy',
      amount: { exactIn: firstBuy, minimumOut: minimumOut(firstQuote.base, 100) },
    })], [launch, mint], tables)
    const poolAddress = addresses.poolAddress(mint.publicKey, NATIVE_MINT)
    const launchAta = addresses.ata(launch.publicKey, mint.publicKey, TOKEN_2022_PROGRAM_ID)
    const initialSupply = (await getMint(connection, mint.publicKey, 'confirmed', TOKEN_2022_PROGRAM_ID)).supply
    const allocation = (await getAccount(connection, launchAta, 'confirmed', TOKEN_2022_PROGRAM_ID)).amount
    assert.ok(allocation > 0n)
    let cpmmAddress = null

    async function curveBuyPlan(wallet, lamports, slippageBps = 300) {
      const pool = await readPoolState(connection, poolAddress)
      assert.equal(pool.status, 0, 'Curve is no longer trading')
      const quoted = buyExactIn(curveState(pool), lamports, rates)
      return { minimumOut: minimumOut(quoted.base, slippageBps), lookupTables: [addresses.RAYDIUM.lookupTable],
        instructions: tradeInstructions({ payer: wallet, pool, side: 'buy', amount: { exactIn: lamports, minimumOut: minimumOut(quoted.base, slippageBps) } }) }
    }

    async function cpmmTradePlan(wallet, inputMint, amountIn, slippageBps = 300) {
      assert.ok(cpmmAddress)
      const first = decodeCpmmPool(cpmmAddress, await connection.getAccountInfo(cpmmAddress, 'confirmed'))
      const [poolInfo, vault0, vault1, configInfo] = await connection.getMultipleAccountsInfo([cpmmAddress, first.vault0, first.vault1, first.ammConfig], 'confirmed')
      const pool = decodeCpmmPool(cpmmAddress, poolInfo), tier = decodeAmmConfig(configInfo)
      const { reserve0, reserve1 } = cpmmReserves(unpackAccount(pool.vault0, vault0, pool.tokenProgram0).amount, unpackAccount(pool.vault1, vault1, pool.tokenProgram1).amount, pool)
      const inputIs0 = inputMint.equals(pool.mint0)
      const quote = swapBaseInput(amountIn, inputIs0 ? reserve0 : reserve1, inputIs0 ? reserve1 : reserve0, cpmmRates(pool, tier), swapPaysCreatorFeeOnInput(pool, inputMint))
      const wsol = addresses.ata(wallet, NATIVE_MINT)
      const instructions = [
        createAssociatedTokenAccountIdempotentInstruction(wallet, addresses.ata(wallet, mint.publicKey, TOKEN_2022_PROGRAM_ID), wallet, mint.publicKey, TOKEN_2022_PROGRAM_ID),
        createAssociatedTokenAccountIdempotentInstruction(wallet, wsol, wallet, NATIVE_MINT),
      ]
      if (inputMint.equals(NATIVE_MINT)) instructions.push(SystemProgram.transfer({ fromPubkey: wallet, toPubkey: wsol, lamports: amountIn }), createSyncNativeInstruction(wsol))
      instructions.push(cpmmSwapInstruction({ payer: wallet, pool, inputMint, amount: { exactIn: amountIn, minimumOut: minimumOut(quote.amountOut, slippageBps) } }), createCloseAccountInstruction(wsol, wallet, wallet))
      return { instructions, lookupTables: [addresses.RAYDIUM.lookupTable], minimumOut: minimumOut(quote.amountOut, slippageBps) }
    }

    await send(connection, (await curveBuyPlan(buyer.publicKey, 20_000_000_000n)).instructions, [buyer], tables)
    const feeVault = addresses.creatorFeeVault(launch.publicKey, NATIVE_MINT)
    assert.ok((await getAccount(connection, feeVault, 'confirmed', TOKEN_PROGRAM_ID)).amount > 0n, 'Another buyer must accrue creator fees')
    const { connectDatabase } = await import('../dist/db.js')
    if (!process.env.DATABASE_URL) throw new Error('DATABASE_URL is required; apply prisma migrations before the rehearsal.')
    database = connectDatabase(process.env.DATABASE_URL)
    const swapper = { instructions: ({ wallet, lamports, slippageBps }) => cpmmAddress ? cpmmTradePlan(wallet, NATIVE_MINT, lamports, slippageBps) : curveBuyPlan(wallet, lamports, slippageBps) }
    const makeService = () => createBuybackService({ config, connection, signer, db: database.db, swapper })
    let service = makeService()
    let totalBurned = 0n, retainedTokens = 0n, carryLamports = 0n
    const receiptCache = new Map()
    const exact = value => { assert.ok(Number.isSafeInteger(value) && value >= 0); return BigInt(value) }

    async function receipt(signature) {
      if (receiptCache.has(signature)) return receiptCache.get(signature)
      const raw = await readTransaction(connectionRpc(connection), signature)
      const tx = decodeTransaction(raw, [])
      assert.ok(tx.valid && !tx.failed && tx.signature === signature)
      const balances = tx.tokenBalances.filter(row => row.mint === mint.publicKey.toBase58() && row.owner === launch.publicKey.toBase58())
      const sum = side => balances.filter(row => row.side === side).reduce((amount, row) => amount + BigInt(row.raw), 0n)
      const delta = sum('post') - sum('pre')
      const walletIndex = tx.keys.findIndex(key => key.address === launch.publicKey.toBase58())
      assert.ok(walletIndex >= 0)
      let createdAccountLamports = 0n
      tx.keys.forEach((key, index) => {
        if (index !== walletIndex && exact(raw.meta.preBalances[index]) === 0n) createdAccountLamports += exact(raw.meta.postBalances[index])
      })
      const result = { raw, tx, delta, fee: exact(raw.meta.fee), createdAccountLamports }
      receiptCache.set(signature, result)
      return result
    }

    async function claimDelivery(signature) {
      const result = await receipt(signature)
      const out = { curveLamports: 0n, cpmmLamports: 0n, cpmmTokens: 0n }
      const wsol = addresses.ata(launch.publicKey, NATIVE_MINT).toBase58()
      for (const ix of result.tx.instructions.filter(ix => ix.parentIndex === null)) {
        const curveClaim = ix.programId === addresses.LAUNCHLAB_PROGRAM_ID.toBase58() && Buffer.from(ix.data).equals(Buffer.from([26, 97, 138, 203, 132, 171, 141, 252]))
        const cpmmClaim = ix.programId === addresses.CPMM_PROGRAM_ID.toBase58() && Buffer.from(ix.data).equals(Buffer.from([202, 202, 34, 83, 226, 122, 145, 229]))
        if (!curveClaim && !cpmmClaim) continue
        for (const flow of descendants(result.tx, ix)) {
          if (flow.kind !== 'transfer') continue
          if (flow.destination === wsol && flow.mint === NATIVE_MINT.toBase58()) out[curveClaim ? 'curveLamports' : 'cpmmLamports'] += BigInt(flow.amountRaw)
          else if (cpmmClaim && flow.destination === launchAta.toBase58() && flow.mint === mint.publicKey.toBase58()) out.cpmmTokens += BigInt(flow.amountRaw)
        }
      }
      assert.equal(result.delta, out.cpmmTokens, 'Claim token amount comes from the receipt balance delta')
      return out
    }

    async function runAndVerify({ expectSwap = true, floorRetry = false, label }) {
      const where = { mint: mint.publicKey.toBase58(), wallet: launch.publicKey.toBase58() }
      const previousRows = await database.db.buybackRow.findMany({ where })
      const walletBefore = BigInt(await connection.getBalance(launch.publicKey, 'confirmed'))
      assert.ok(walletBefore > 3_000_000_000n, 'Keep several SOL of unrelated wallet funds above the floor')
      let floorReservation = null
      if (floorRetry) {
        const waitingCurveFees = (await getAccount(connection, feeVault, 'confirmed', TOKEN_PROGRAM_ID)).amount
        assert.ok(waitingCurveFees >= config.minSwapLamports)
        // Claim income clears this floor; spending that exact claim would put the wallet below it.
        const highFloorConfig = { ...config, solFloorLamports: walletBefore + waitingCurveFees / 2n }
        const floorService = () => createBuybackService({ config: highFloorConfig, connection, signer, db: database.db, swapper })
        const broadcasts = new Set()
        const originalSend = connection.sendRawTransaction
        connection.sendRawTransaction = function (bytes, options) {
          broadcasts.add(signatureOf(VersionedTransaction.deserialize(bytes)))
          return originalSend.call(this, bytes, options)
        }
        try {
          await assert.rejects(floorService().runCycle(), /wallet SOL floor or reserved fees/)
          const claimedRows = await database.db.buybackRow.findMany({ where, orderBy: { createdAt: 'asc' } })
          const newlyClaimed = claimedRows.slice(previousRows.length)
          assert.deepEqual(newlyClaimed.map(row => row.action), ['claim'])
          const claim = newlyClaimed[0], delivered = await claimDelivery(claim.signature)
          const expected = carryLamports + delivered.curveLamports + delivered.cpmmLamports * BigInt(config.postGraduationBps) / 10_000n
          const pending = await database.db.buybackCycle.findUniqueOrThrow({ where: { id: claim.cycleId } })
          assert.equal(pending.state, 'swap')
          assert.equal(BigInt(pending.swapLamports.toString()), expected, 'Floor refusal retains the entire exact claim reservation')
          floorReservation = expected
          assert.equal(await database.db.keeperTransaction.findUnique({ where: { key: `buyback:${pending.id}:swap` } }), null, 'A refused swap is never signed or journaled')
          // A new service must retry the same reservation rather than reducing it to fit the floor.
          await assert.rejects(floorService().runCycle(), /wallet SOL floor or reserved fees/)
          const retried = await database.db.buybackCycle.findUniqueOrThrow({ where: { id: pending.id } })
          assert.equal(retried.swapLamports.toString(), pending.swapLamports.toString())
          assert.equal(await database.db.buybackRow.count({ where }), claimedRows.length)
          assert.equal(await database.db.keeperTransaction.findUnique({ where: { key: `buyback:${pending.id}:swap` } }), null)
          assert.deepEqual([...broadcasts], [claim.signature], 'Only the claim signature may be broadcast while the floor refuses the swap')
        } finally { connection.sendRawTransaction = originalSend }
        service = makeService() // Restore the normal floor and resume the persisted reservation.
      }
      await service.runCycle()
      const rows = await database.db.buybackRow.findMany({ where, orderBy: { createdAt: 'asc' } })
      const recent = rows.slice(previousRows.length)
      const claim = recent.find(row => row.action === 'claim')
      assert.ok(claim, 'A real creator-fee transaction must be recorded')
      const parts = await claimDelivery(claim.signature)
      const claimSol = parts.curveLamports + parts.cpmmLamports
      const selectedSol = parts.curveLamports + parts.cpmmLamports * BigInt(config.postGraduationBps) / 10_000n
      const selectedTokens = parts.cpmmTokens * BigInt(config.postGraduationBps) / 10_000n
      const available = carryLamports + selectedSol
      assert.equal(BigInt(claim.lamports.toString()), claimSol, 'Claim rows contain actual SOL transferred, excluding WSOL rent')
      assert.equal(BigInt(claim.tokenAmount.toString()), parts.cpmmTokens, 'Claim row contains all ACRUX actually received')
      assert.ok(recent.every(row => row.signature.length > 0 && row.cycleId === claim.cycleId))
      const swap = recent.find(row => row.action === 'swap'), burn = recent.find(row => row.action === 'burn')
      if (expectSwap) {
        assert.ok(available >= config.minSwapLamports)
        assert.ok(swap)
        assert.equal(BigInt(swap.lamports.toString()), available, 'Swap only the persisted claim-funded budget, without unrelated wallet SOL')
        if (floorReservation !== null) assert.equal(BigInt(swap.lamports.toString()), floorReservation, 'Restart spends the unchanged reservation after the floor permits it')
        assert.equal(BigInt(swap.tokenAmount.toString()), (await receipt(swap.signature)).delta)
        carryLamports = 0n
      } else {
        assert.ok(available > 0n && available < config.minSwapLamports, 'Exercise a real sub-minimum carry')
        assert.equal(swap, undefined)
        carryLamports = available
      }
      const delivered = swap ? (await receipt(swap.signature)).delta : 0n
      const burnAmount = delivered + selectedTokens
      assert.deepEqual(recent.map(row => row.action), ['claim', ...(expectSwap ? ['swap'] : []), ...(burnAmount > 0n ? ['burn'] : [])])
      if (burnAmount > 0n) {
        assert.ok(burn)
        assert.equal(BigInt(burn.tokenAmount.toString()), burnAmount, 'Burn swap delivery plus the selected CPMM token share')
        assert.equal((await receipt(burn.signature)).delta, -burnAmount)
      } else assert.equal(burn, undefined)
      totalBurned += burnAmount
      retainedTokens += parts.cpmmTokens - selectedTokens
      assert.equal((await getAccount(connection, launchAta, 'confirmed', TOKEN_2022_PROGRAM_ID)).amount, allocation + retainedTokens, 'Preserve first-buy allocation and unselected CPMM ACRUX')
      const walletAfter = BigInt(await connection.getBalance(launch.publicKey, 'confirmed'))
      const receipts = await Promise.all(recent.map(row => receipt(row.signature)))
      const transactionFees = receipts.reduce((amount, tx) => amount + tx.fee, 0n)
      const accountCreationCosts = receipts.reduce((amount, tx) => amount + tx.createdAccountLamports, 0n)
      assert.ok(walletBefore - walletAfter <= (swap ? BigInt(swap.lamports.toString()) : 0n) + transactionFees + accountCreationCosts, 'Only the selected swap, transaction fees and account rent may reduce wallet SOL')
      assert.ok(walletAfter >= config.solFloorLamports)
      assert.equal(initialSupply - (await getMint(connection, mint.publicKey, 'confirmed', TOKEN_2022_PROGRAM_ID)).supply, totalBurned)
      const budget = await database.db.buybackBudget.findUnique({ where: { mint_wallet: { mint: where.mint, wallet: where.wallet } } })
      assert.equal(BigInt(budget?.owedLamports.toString() ?? '0'), carryLamports, 'Persist sub-minimum funds across service restarts')
      const journal = await database.db.keeperTransaction.findMany({ where: { key: { startsWith: `buyback:${claim.cycleId}:` } } })
      assert.equal(journal.length, recent.length)
      assert.ok(journal.every(row => row.state === 'confirmed'))
      await withApi(mint.publicKey.toBase58(), launch.publicKey.toBase58(), body => {
        const sum = (action, field) => rows.filter(row => row.action === action).reduce((value, row) => value + BigInt(row[field].toString()), 0n).toString()
        assert.equal(body.status, 'live')
        assert.equal(body.totals.claimedLamports, sum('claim', 'lamports'))
        assert.equal(body.totals.swappedLamports, sum('swap', 'lamports'))
        assert.equal(body.totals.acruxBought, sum('swap', 'tokenAmount'))
        assert.equal(body.totals.acruxBurned, totalBurned.toString())
        assert.equal(body.rows.length, rows.length)
      })
      console.log(`PASS: ${label}`)
      return { claim, swap, burn, parts }
    }

    assert.equal(config.postGraduationBps, 2000)
    const first = await runAndVerify({ floorRetry: true, label: 'floor refusal preserves the exact curve claim reservation across retries and restart, then swaps it at 100%' })
    assert.equal(first.parts.cpmmLamports, 0n)
    // The keeper's own buy has accrued a small new curve fee below the 50m minimum.
    await runAndVerify({ expectSwap: false, label: 'small curve claim is carried without swapping unrelated wallet SOL' })
    const carried = carryLamports
    assert.ok(carried > 0n)
    service = makeService() // Reconstruct the keeper; only the database carries the prior claim budget.
    await send(connection, (await curveBuyPlan(buyer.publicKey, 20_000_000_000n)).instructions, [buyer], tables)
    const resumed = await runAndVerify({ label: 'restarted keeper aggregates the persisted carry with a later creator claim' })
    assert.equal(BigInt(resumed.swap.lamports.toString()), carried + resumed.parts.curveLamports)

    const beforeFinish = await readPoolState(connection, poolAddress)
    const finish = completionQuote(curveState(beforeFinish), rates)
    await send(connection, tradeInstructions({ payer: buyer.publicKey, pool: beforeFinish, side: 'buy', amount: { exactIn: finish.quote, minimumOut: finish.base } }), [buyer], tables)
    const finished = await readPoolState(connection, poolAddress)
    assert.equal(finished.status, 1, 'Completion buy must sell the curve out')
    const feeNft = Keypair.generate()
    const migrationSignature = await send(connection, [migrateToCpswapInstruction({ payer: crank.publicKey, pool: finished, platform, feeNftMint: feeNft.publicKey })], [crank, feeNft], tables)
    const migrationTx = await connection.getTransaction(migrationSignature, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })
    assert.ok(migrationTx?.meta && !migrationTx.meta.err)
    cpmmAddress = migratedPoolFromTransaction(migrationTx, poolAddress)
    assert.ok(cpmmAddress, 'Read the actual CPMM address from the migration transaction')
    const graduated = decodeCpmmPool(cpmmAddress, await connection.getAccountInfo(cpmmAddress, 'confirmed'))
    assert.ok(graduated.poolCreator.equals(launch.publicKey), 'Platform must keep the launch wallet as CPMM creator')
    assert.equal(graduated.creatorFeeOn, 0, 'Both-token creator fees exercise the token-claim path')
    const clockDeadline = Date.now() + 30_000
    for (;;) {
      const clock = await connection.getAccountInfo(SYSVAR_CLOCK_PUBKEY, 'confirmed')
      assert.ok(clock && clock.data.length >= 40)
      if (clock.data.readBigInt64LE(32) >= graduated.openTime) break
      assert.ok(Date.now() < clockDeadline, 'Localnet clock did not reach CPMM open_time')
      await pause(250)
    }
    await send(connection, (await cpmmTradePlan(buyer.publicKey, NATIVE_MINT, 20_000_000_000n)).instructions, [buyer], tables)
    const buyerAta = addresses.ata(buyer.publicKey, mint.publicKey, TOKEN_2022_PROGRAM_ID)
    const sale = (await getAccount(connection, buyerAta, 'confirmed', TOKEN_2022_PROGRAM_ID)).amount / 100n
    await send(connection, (await cpmmTradePlan(buyer.publicKey, mint.publicKey, sale)).instructions, [buyer], tables)
    const snapshot = await readFeeSnapshot(connection, { platformConfig, creator: launch.publicKey, quoteMint: NATIVE_MINT, cpmmPool: cpmmAddress })
    assert.ok(snapshot.cpmm && snapshot.cpmm.creatorPayout0 > 0n && snapshot.cpmm.creatorPayout1 > 0n, 'CPMM trades must book creator fees in both tokens')
    const baseIs0 = snapshot.cpmm.pool.mint0.equals(mint.publicKey)
    const claimTokens = baseIs0 ? snapshot.cpmm.creatorPayout0 : snapshot.cpmm.creatorPayout1
    const claimLamports = snapshot.creatorVault + (baseIs0 ? snapshot.cpmm.creatorPayout1 : snapshot.cpmm.creatorPayout0)
    const cpmm = await runAndVerify({ label: 'mixed curve and CPMM claim applies 100% curve plus 20% CPMM shares, preserving unselected ACRUX and wallet SOL' })
    assert.equal(cpmm.parts.cpmmTokens, claimTokens)
    assert.equal(BigInt(cpmm.claim.lamports.toString()), claimLamports, 'Record net creator fee after Raydium share, excluding WSOL rent')
    assert.ok(cpmm.parts.curveLamports > 0n && cpmm.parts.cpmmLamports > 0n, 'Exercise the mixed claim source path')
    assert.equal(BigInt(cpmm.swap.lamports.toString()), cpmm.parts.curveLamports + cpmm.parts.cpmmLamports * 2000n / 10_000n)
    assert.ok(retainedTokens > 0n)

    // Size one more local trade so its net creator payout alone clears the default swap minimum.
    const cpmmTier = decodeAmmConfig(await connection.getAccountInfo(ammConfig, 'confirmed'))
    assert.ok(cpmmTier.creatorFeeRate > 0n && snapshot.cpmm.shareRate < 1_000_000n)
    const wantedPayout = ceilDiv(config.minSwapLamports * 10_000n, BigInt(config.postGraduationBps)) * 12n / 10n
    const bookedFee = ceilDiv(wantedPayout * 1_000_000n, 1_000_000n - snapshot.cpmm.shareRate)
    const cpmmSolInput = ceilDiv(bookedFee * 1_000_000n, cpmmTier.creatorFeeRate)
    const buyerBalance = BigInt(await connection.getBalance(buyer.publicKey, 'confirmed'))
    if (buyerBalance < cpmmSolInput + 10_000_000n) {
      const shortfall = cpmmSolInput + 10_000_000n - buyerBalance
      assert.ok(shortfall <= BigInt(Number.MAX_SAFE_INTEGER))
      await fund(connection, buyer.publicKey, Number(shortfall))
    }
    await send(connection, (await cpmmTradePlan(buyer.publicKey, NATIVE_MINT, cpmmSolInput)).instructions, [buyer], tables)
    const nextSale = (await getAccount(connection, buyerAta, 'confirmed', TOKEN_2022_PROGRAM_ID)).amount / 100n
    await send(connection, (await cpmmTradePlan(buyer.publicKey, mint.publicKey, nextSale)).instructions, [buyer], tables)
    const cpmmOnly = await runAndVerify({ label: 'CPMM-only claim swaps exactly 20% of delivered SOL and burns only its selected token share plus swap delivery' })
    assert.equal(cpmmOnly.parts.curveLamports, 0n)
    assert.ok(cpmmOnly.parts.cpmmLamports > 0n && cpmmOnly.parts.cpmmTokens > 0n)
    assert.equal(BigInt(cpmmOnly.swap.lamports.toString()), cpmmOnly.parts.cpmmLamports * 2000n / 10_000n)
    // Rehearsal rows are isolated by random mint and wallet, and remain available for inspection.
  } finally {
    await database?.close()
    await rm(folder, { recursive: true, force: true })
  }
}

async function main() {
  await checkWaiting()
  if (!process.argv.includes('--waiting-only')) await runLocalnet()
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => {
    let message = error instanceof Error ? error.stack ?? error.message : 'Buyback rehearsal failed'
    for (const value of [process.env.SOLANA_MAINNET_RPC_URL, process.env.SOLANA_RPC_URL, process.env.DATABASE_URL]) if (value) message = message.replaceAll(value, '[redacted]')
    console.error(message)
    process.exitCode = 1
  })
}
