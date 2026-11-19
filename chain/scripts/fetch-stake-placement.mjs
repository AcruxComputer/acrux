// Road build 4 step 3: Solana's Stake program, taken from mainnet once into
// chain/placements/stake.json the way fetch-placements.mjs takes the others: the program and its
// program data, deployed in slot 0, no upgrade authority, each account at Acrux's rent-exempt minimum.
// Usage (from backend/chain): node scripts/fetch-stake-placement.mjs <mainnet rpc url> [acrux rpc url]
import { writeFileSync } from 'node:fs'
import { Connection, PublicKey } from '@solana/web3.js'

const [mainnetUrl, acruxUrl = 'https://rpc.acrux.computer'] = process.argv.slice(2)
const mainnet = new Connection(mainnetUrl, 'finalized')
const acrux = new Connection(acruxUrl, 'confirmed')
const stake = new PublicKey('Stake11111111111111111111111111111111111111')

const entry = async (address, info, data = info.data) => ({
  address: address.toBase58(), owner: info.owner.toBase58(), executable: info.executable,
  lamports: await acrux.getMinimumBalanceForRentExemption(data.length), data: Buffer.from(data).toString('base64'),
})
const account = await mainnet.getAccountInfo(stake)
if (account?.owner.toBase58() !== 'BPFLoaderUpgradeab1e11111111111111111111111') throw new Error(`The Stake program on mainnet is owned by ${account?.owner.toBase58()}, not the upgradeable loader.`)
const programData = new PublicKey(account.data.subarray(4, 36))
const dataInfo = await mainnet.getAccountInfo(programData)
const data = Buffer.from(dataInfo.data)
data.writeBigUInt64LE(0n, 4) // deployed in slot 0, visible from the placement on
data.fill(0, 12, 45) // upgrade authority: none
const accounts = [await entry(programData, dataInfo, data), await entry(stake, account)]
writeFileSync(new URL('../placements/stake.json', import.meta.url), JSON.stringify({ name: 'Stake', accounts }, null, 1))
console.log(`Stake: ${dataInfo.data.length} bytes of program data, written to placements/stake.json`)
