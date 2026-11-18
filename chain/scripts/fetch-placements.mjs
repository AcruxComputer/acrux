// Takes the accounts of the programs Acrux places at their mainnet addresses (road build 1, step 7)
// from Solana mainnet, once, into chain/placements/<name>.json: each upgradeable program's program
// and program data accounts, with the program data's deploy slot set to 0 and its upgrade authority
// removed (no one upgrades a placed program on Acrux), plus the accounts the program needs to run.
// Every account is funded at Acrux's own rent-exempt minimum.
// Usage (from backend/sdk, where @solana/web3.js is installed):
//   node ../chain/scripts/fetch-placements.mjs <mainnet rpc url> [acrux rpc url]
import { writeFileSync } from 'node:fs'
import { Connection, PublicKey } from '@solana/web3.js'
import { createHash } from 'node:crypto'

const [mainnetUrl, acruxUrl = 'https://rpc.acrux.computer'] = process.argv.slice(2)
const mainnet = new Connection(mainnetUrl, 'finalized')
const acrux = new Connection(acruxUrl, 'confirmed')
const out = new URL('../placements/', import.meta.url)
const rent = async size => acrux.getMinimumBalanceForRentExemption(size)

const entry = async (address, info, data = info.data) => ({
  address: address.toBase58(),
  owner: info.owner.toBase58(),
  executable: info.executable,
  lamports: await rent(data.length),
  data: Buffer.from(data).toString('base64'),
})

async function program(id) {
  const account = await mainnet.getAccountInfo(id)
  const programData = new PublicKey(account.data.subarray(4, 36))
  const data = Buffer.from((await mainnet.getAccountInfo(programData)).data)
  data.writeBigUInt64LE(0n, 4) // deployed in slot 0, visible from the placement on
  data.fill(0, 12, 45) // upgrade authority: none
  return [await entry(programData, await mainnet.getAccountInfo(programData), data), await entry(id, account)]
}

// Mainnet's current memo program, the one @solana/spl-memo 0.3 sends to.
const memo = new PublicKey('Memo4c2pN8afCj432Lb7RMVKi9PbQnnW7ewFFaV3oAH')
writeFileSync(new URL('memo-4c2p.json', out), JSON.stringify({ name: 'Memo (Memo4c2p)', accounts: await program(memo) }, null, 1))

// Raydium CPMM: the program, every AmmConfig (its fee tiers), and the pool-creation fee receiver,
// a wrapped-SOL account its create instruction pays into, placed empty.
const cpmm = new PublicKey('CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C')
const discriminator = createHash('sha256').update('account:AmmConfig').digest().subarray(0, 8)
const configs = await mainnet.getProgramAccounts(cpmm, { filters: [{ memcmp: { offset: 0, bytes: (await import('bs58')).default.encode(discriminator) } }] })
const feeReceiver = new PublicKey('DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8')
const receiver = await mainnet.getAccountInfo(feeReceiver)
const receiverData = Buffer.from(receiver.data)
const receiverRent = await rent(receiverData.length)
receiverData.writeBigUInt64LE(0n, 64) // amount
receiverData.writeBigUInt64LE(BigInt(receiverRent), 113) // is_native: Some(rent)
const accounts = [
  ...(await Promise.all(configs.map(c => entry(c.pubkey, c.account)))),
  await entry(feeReceiver, receiver, receiverData),
  ...(await program(cpmm)), // the program account last
]
writeFileSync(new URL('raydium-cpmm.json', out), JSON.stringify({ name: 'Raydium CPMM', accounts }, null, 1))
console.log(`memo: 2 accounts; cpmm: ${accounts.length} accounts (${configs.length} AmmConfigs)`)
