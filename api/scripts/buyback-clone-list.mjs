import assert from 'node:assert/strict'
import { mkdir, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { AddressLookupTableAccount, AddressLookupTableProgram, Connection, Keypair, PublicKey, SystemProgram, ComputeBudgetProgram } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, ASSOCIATED_TOKEN_PROGRAM_ID, NATIVE_MINT } from '@solana/spl-token'
import * as addresses from '../dist/engines/launchpad/launchlab/addresses.js'
import { decodeGlobalConfig } from '../dist/engines/launchpad/launchlab/accounts.js'
import { initializeWithToken2022Instruction, launchedPool } from '../dist/engines/launchpad/launchlab/launch.js'
import { createPlatformConfigInstruction } from '../dist/engines/launchpad/launchlab/platform.js'
import { tradeInstructions } from '../dist/engines/launchpad/launchlab/trade.js'
import { claimCreatorFeeInstruction, unwrapAfterClaim } from '../dist/engines/launchpad/launchlab/fees.js'
import { migrateToCpswapInstruction } from '../dist/engines/launchpad/launchlab/migration.js'
import { collectCreatorFeePermissionlessInstruction, cpmmSwapInstruction, decodeAmmConfig } from '../dist/engines/launchpad/launchlab/cpmm.js'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const backend = path.resolve(root, '..')
export const defaultPrepDirectory = path.join(root, 'node_modules', '.cache', 'buyback-localnet')
export const ammConfigAddress = index => {
  const bytes = Buffer.alloc(2)
  bytes.writeUInt16BE(index)
  return addresses.pda(addresses.CPMM_PROGRAM_ID, 'amm_config', bytes)
}

/** Build every flow offline with random coin-specific addresses; nothing here reads chain state. */
export function buildCloneInstructions(ammConfig = ammConfigAddress(0)) {
  const wallet = Keypair.generate().publicKey, mint = Keypair.generate().publicKey, nft = Keypair.generate().publicKey
  const platformConfig = addresses.platformConfigAddress(wallet)
  const launch = {
    payer: wallet, baseMint: mint, globalConfig: addresses.globalConfigAddress(NATIVE_MINT), quoteMint: NATIVE_MINT, platformConfig,
    token: { name: 'Acrux keeper rehearsal', symbol: 'AKPR', uri: 'https://example.invalid/acrux-keeper.json' },
    curve: { supply: 1_000_000_000_000_000n, totalBaseSell: 793_100_000_000_000n, totalQuoteFundRaising: 85_000_000_000n },
    ammCreatorFeeOn: 'bothToken',
  }
  const pool = launchedPool(launch, { token2022: true })
  const cp = addresses.cpmmPoolAddresses(ammConfig, mint, NATIVE_MINT)
  const baseIs0 = cp.mint0.equals(mint)
  const cpmm = {
    address: cp.pool, ammConfig, poolCreator: wallet, vault0: cp.vault0, vault1: cp.vault1, lpMint: cp.lpMint,
    mint0: cp.mint0, mint1: cp.mint1, tokenProgram0: baseIs0 ? TOKEN_2022_PROGRAM_ID : TOKEN_PROGRAM_ID,
    tokenProgram1: baseIs0 ? TOKEN_PROGRAM_ID : TOKEN_2022_PROGRAM_ID, observation: cp.observation,
  }
  return [
    createPlatformConfigInstruction({ admin: wallet, feeWallet: wallet, nftWallet: wallet, cpswapConfig: ammConfig,
      migrateNftInfo: { platformScale: 0n, creatorScale: 0n, burnScale: 1_000_000n },
      feeRate: 0n, creatorFeeRate: 5_000n, name: 'Keeper rehearsal', web: '', img: '' }),
    initializeWithToken2022Instruction(launch),
    ...tradeInstructions({ payer: wallet, pool, side: 'buy', amount: { exactIn: 100_000_000n, minimumOut: 1n } }),
    claimCreatorFeeInstruction({ creator: wallet, quoteMint: NATIVE_MINT }), ...unwrapAfterClaim(wallet, NATIVE_MINT),
    migrateToCpswapInstruction({ payer: wallet, pool, platform: { cpswapConfig: ammConfig, platformNftWallet: wallet, platformCpCreator: PublicKey.default }, feeNftMint: nft }),
    cpmmSwapInstruction({ payer: wallet, pool: cpmm, inputMint: NATIVE_MINT, amount: { exactIn: 100_000_000n, minimumOut: 1n } }),
    cpmmSwapInstruction({ payer: wallet, pool: cpmm, inputMint: mint, amount: { exactIn: 100_000n, minimumOut: 1n } }),
    collectCreatorFeePermissionlessInstruction(wallet, cpmm),
  ]
}

export function cloneCandidates(instructions, ammConfig = ammConfigAddress(0)) {
  const keys = new Map()
  const add = key => keys.set(key.toBase58(), key)
  for (const instruction of instructions) for (const key of [instruction.programId, ...instruction.keys.map(meta => meta.pubkey)]) add(key)
  for (const key of [
    addresses.LAUNCHLAB_PROGRAM_ID, addresses.CPMM_PROGRAM_ID, addresses.LP_LOCK_PROGRAM_ID,
    addresses.METADATA_PROGRAM_ID, addresses.MEMO_PROGRAM_ID, TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID,
    ASSOCIATED_TOKEN_PROGRAM_ID, NATIVE_MINT, addresses.globalConfigAddress(NATIVE_MINT), ammConfig, addresses.RAYDIUM.lookupTable,
  ]) add(key)
  const builtins = new Set([SystemProgram.programId.toBase58(), ComputeBudgetProgram.programId.toBase58()])
  return [...keys.values()].filter(key => !builtins.has(key.toBase58()) && !key.toBase58().startsWith('Sysvar'))
}

function outputDirectory() {
  const index = process.argv.indexOf('--out-dir')
  if (index !== -1 && !process.argv[index + 1]) throw new Error('--out-dir needs a directory inside backend')
  const directory = path.resolve(index === -1 ? defaultPrepDirectory : process.argv[index + 1])
  const relative = path.relative(backend, directory)
  if (relative === '' || relative.startsWith('..') || path.isAbsolute(relative)) throw new Error('Clone preparation directory must be inside backend')
  return directory
}

const unsigned = value => { assert.ok(Number.isSafeInteger(value) && value >= 0, 'RPC returned inexact lamports'); return value }
function accountJson(pubkey, info, data = info.data) {
  return { pubkey: pubkey.toBase58(), account: { lamports: unsigned(info.lamports), data: [data.toString('base64'), 'base64'], owner: info.owner.toBase58(), executable: info.executable, rentEpoch: 0 } }
}
const shell = value => `'${value.replaceAll("'", "'\\''")}'`
const wslPath = value => value.replace(/^([A-Za-z]):[\\/]/, (_, drive) => `/mnt/${drive.toLowerCase()}/`).replaceAll('\\', '/')

async function main() {
  const url = process.env.SOLANA_MAINNET_RPC_URL
  if (!url) throw new Error('Set SOLANA_MAINNET_RPC_URL; clone preparation only reads mainnet.')
  const connection = new Connection(url, 'confirmed')
  const globalKey = addresses.globalConfigAddress(NATIVE_MINT)
  const [globalInfo, lookupInfo] = await connection.getMultipleAccountsInfo([globalKey, addresses.RAYDIUM.lookupTable])
  const global = decodeGlobalConfig(globalInfo)
  assert.ok(global.quoteMint.equals(NATIVE_MINT), 'GlobalConfig must quote SOL')
  assert.ok(global.migrateToCpswapWallet.equals(addresses.RAYDIUM.migrationCrank), 'GlobalConfig does not name Raydium migration crank')
  assert.ok(globalInfo.data.subarray(211, 243).equals(addresses.RAYDIUM.migrationCrank.toBuffer()), 'GlobalConfig migration crank bytes changed')
  assert.ok(lookupInfo?.owner.equals(AddressLookupTableProgram.programId), 'Raydium lookup table owner mismatch')
  const lookup = AddressLookupTableAccount.deserialize(lookupInfo.data)
  assert.equal(lookupInfo.data.readBigUInt64LE(12), BigInt(lookup.lastExtendedSlot), 'Lookup table extension offset changed')
  assert.ok(lookup.addresses.length > 0 && lookupInfo.data.readUInt32LE(0) === 1, 'Invalid Raydium lookup table layout')

  // Finite PDA reads, never getProgramAccounts. A live creator-fee tier is required for the second cycle.
  const tiers = Array.from({ length: 32 }, (_, index) => ammConfigAddress(index))
  const tierInfos = await connection.getMultipleAccountsInfo(tiers)
  const tier = tierInfos.map((info, index) => info ? { address: tiers[index], config: decodeAmmConfig(info) } : null)
    .find(value => value && !value.config.disableCreatePool && value.config.creatorFeeRate > 0n && value.config.creatorFeeShareRate < 1_000_000n)
  assert.ok(tier, 'No creator-fee CPMM tier found among config indices 0..31')
  const candidates = cloneCandidates(buildCloneInstructions(tier.address), tier.address)
  const accounts = []
  for (let start = 0; start < candidates.length; start += 100) {
    const batch = candidates.slice(start, start + 100)
    const infos = await connection.getMultipleAccountsInfo(batch)
    infos.forEach((info, index) => { if (info) accounts.push({ pubkey: batch[index], info }) })
  }

  const directory = outputDirectory()
  await mkdir(directory, { recursive: true })
  const crank = Keypair.generate()
  const globalData = Buffer.from(globalInfo.data)
  crank.publicKey.toBuffer().copy(globalData, 211)
  assert.ok(decodeGlobalConfig({ ...globalInfo, data: globalData }).migrateToCpswapWallet.equals(crank.publicKey))
  const lookupData = Buffer.from(lookupInfo.data)
  lookupData.writeBigUInt64LE(0n, 12)
  assert.equal(AddressLookupTableAccount.deserialize(lookupData).lastExtendedSlot, 0)
  const globalFile = path.join(directory, 'global-config.json'), lookupFile = path.join(directory, 'raydium-lookup-table.json')
  const crankFile = path.join(directory, 'migration-crank.json')
  const metadata = { globalConfig: globalKey.toBase58(), ammConfig: tier.address.toBase58(), lookupTable: addresses.RAYDIUM.lookupTable.toBase58(), crankFile: 'migration-crank.json' }
  await writeFile(globalFile, JSON.stringify(accountJson(globalKey, globalInfo, globalData)) + '\n')
  await writeFile(lookupFile, JSON.stringify(accountJson(addresses.RAYDIUM.lookupTable, lookupInfo, lookupData)) + '\n')
  await writeFile(crankFile, JSON.stringify(Array.from(crank.secretKey)) + '\n', { mode: 0o600 })
  await writeFile(path.join(directory, 'metadata.json'), JSON.stringify(metadata, null, 2) + '\n')
  const replaced = new Set([globalKey.toBase58(), addresses.RAYDIUM.lookupTable.toBase58()])
  const upgradeableLoader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111')
  const argumentsList = accounts.filter(({ pubkey }) => !replaced.has(pubkey.toBase58())).map(({ pubkey, info }) => {
    const flag = info.executable && info.owner.equals(upgradeableLoader) && !pubkey.equals(ASSOCIATED_TOKEN_PROGRAM_ID) ? '--clone-upgradeable-program' : '--clone'
    return `${flag} ${pubkey.toBase58()}`
  })
  argumentsList.push(`--account ${globalKey.toBase58()} ${shell(wslPath(globalFile))}`, `--account ${addresses.RAYDIUM.lookupTable.toBase58()} ${shell(wslPath(lookupFile))}`)
  console.log('# Run in WSL with a current Agave validator on a dedicated fresh ledger.')
  console.log('# The two local fixtures patch only the migration crank and lookup-table extension slot.')
  console.log('# Local migration key is stored mode 600 in the preparation directory; no secret is printed.')
  console.log('solana-test-validator --reset --ledger /tmp/acrux-buyback-ledger --rpc-port 5340 --faucet-port 5343 --gossip-port 5344 --dynamic-port-range 5350-5370 --url "$SOLANA_MAINNET_RPC_URL" \\\n  --account-index spl-token-owner --account-index spl-token-mint --account-index program-id \\\n  --mint ' + crank.publicKey.toBase58() + ' \\\n' + argumentsList.map(argument => `  ${argument}`).join(' \\\n'))
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error instanceof Error ? error.message.replaceAll(process.env.SOLANA_MAINNET_RPC_URL ?? '\0', '[RPC]') : 'Clone preparation failed'); process.exitCode = 1 })
}
