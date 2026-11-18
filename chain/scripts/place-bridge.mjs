// Writes acrux_bridge as a placement for Acrux (`chain/placements/` format): the program and its
// program data under the upgradeable loader, deployed in slot 0 with no upgrade authority, at the
// address it holds on Solana, so `withdraw` runs on Acrux beside the mints the bridge credits.
// Usage: node scripts/place-bridge.mjs <out.json> [program .so]
import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { PublicKey } from '@solana/web3.js'
import { chainDir } from './dev-keys.mjs'

const [out, so = join(chainDir, 'target/deploy/acrux_bridge.so')] = process.argv.slice(2)
const program = new PublicKey(JSON.parse(readFileSync(join(chainDir, 'target/idl/acrux_bridge.json'), 'utf8')).address)
const loader = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111')
const programData = PublicKey.findProgramAddressSync([program.toBuffer()], loader)[0]
// Solana's rent-exempt minimum, which Acrux keeps: (128 + length) bytes for two years.
const rent = (length) => (128 + length) * 6960
const elf = readFileSync(so)
// ProgramData: tag 3, deploy slot, then the upgrade authority's 33 bytes, here none; the ELF follows.
const dataHeader = Buffer.alloc(45)
dataHeader.writeUInt32LE(3, 0)
const data = Buffer.concat([dataHeader, elf])
// Program: tag 2 and its program data's address.
const programAccount = Buffer.concat([Buffer.from([2, 0, 0, 0]), programData.toBuffer()])
const account = (address, bytes, executable) => ({ address: address.toBase58(), owner: loader.toBase58(), executable, lamports: rent(bytes.length), data: bytes.toString('base64') })
writeFileSync(out, JSON.stringify({ name: 'Acrux bridge', accounts: [account(programData, data, false), account(program, programAccount, true)] }, null, 1))
console.log(`${program.toBase58()}: ${elf.length} bytes of program, written to ${out}`)
