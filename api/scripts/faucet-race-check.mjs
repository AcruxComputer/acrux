// B2: parallel faucet requests are counted against each other, and the route is
// limited. Starts a stand-in for the node's gateway on 5065 (each grant executes 1 s after it is
// submitted, as on the live net) and the API in node mode on 5066 against DATABASE_URL from .env.
// Usage, from backend/api: node scripts/faucet-race-check.mjs
import { createServer } from 'node:http'
import { spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { balanceFixtures, transactionsByState } from '@acrux/contract/fixtures'
import { toBase58 } from '@acrux/contract'

const STUB = 5065, API = 5066
const paid = []
const submitted = new Map()
const base58 = n => toBase58(randomBytes(n))
const json = (res, status, body) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)) }
const stub = createServer(async (req, res) => {
  let body = ''
  for await (const chunk of req) body += chunk
  if (req.url === '/health') return json(res, 200, { status: 'ready' })
  if (req.url === '/internal/faucet' && req.method === 'GET') return json(res, 200, { availableLamports: '1000000000000' })
  if (req.url === '/internal/faucet') {
    // As the node builds it: one address in one 50 ms slot is one transaction, paid once.
    const { address } = JSON.parse(body), id = createHash('sha256').update(`${address}:${Math.floor(Date.now() / 50)}`).digest('hex')
    if (!submitted.has(id)) { paid.push(address); submitted.set(id, Date.now()) }
    return json(res, 200, { transactionId: id, signature: base58(64), balance: { ...balanceFixtures.normal, address } })
  }
  const call = JSON.parse(body)
  if (call.method === 'getTransaction') {
    const at = submitted.get(call.params.id)
    const tx = Date.now() - at > 1000 ? transactionsByState.executed : transactionsByState.sent
    return json(res, 200, { jsonrpc: '2.0', id: call.id, result: tx })
  }
  if (call.method === 'getBalance') return json(res, 200, { jsonrpc: '2.0', id: call.id, result: { ...balanceFixtures.normal, address: call.params.address } })
  json(res, 200, { jsonrpc: '2.0', id: call.id, error: { code: -32601, message: 'stand-in' } })
})
await new Promise(resolve => stub.listen(STUB, '127.0.0.1', resolve))

const stand = `http://127.0.0.1:${STUB}`
const api = spawn(process.execPath, ['--env-file=.env', 'node_modules/tsx/dist/cli.mjs', 'src/server.ts'], {
  env: { ...process.env, MODE: 'node', PORT: String(API), GATEWAY_RPC_URL: stand, GATEWAY_INTERNAL_URL: stand, SOLANA_RPC_URL: stand,
    FAUCET_PER_KEY_LIMIT: '3', FAUCET_PER_IP_LIMIT: '10' },
  stdio: ['ignore', 'ignore', 'inherit'],
})
const url = `http://127.0.0.1:${API}`
for (let i = 0; ; i++) {
  if (await fetch(`${url}/health`).then(() => true, () => false)) break
  if (i > 120) throw new Error('API did not start')
  await new Promise(r => setTimeout(r, 500))
}
const ip = () => `198.51.100.${randomBytes(1)[0]}`
const grant = (address, from) => fetch(`${url}/v1/faucet`, { method: 'POST', headers: { 'content-type': 'application/json', 'x-forwarded-for': from }, body: JSON.stringify({ address }) })
  .then(async r => r.status === 200 ? 'granted' : (await r.json()).error?.code ?? r.status)
const tally = list => Object.entries(list.reduce((t, x) => ({ ...t, [x]: (t[x] ?? 0) + 1 }), {})).map(([k, v]) => `${k} ${v}`).join(', ')
let ok = true

// One key, 8 requests at once from 8 addresses: paid exactly its limit of 3, each answered as
// granted with its own transaction, the rest FAUCET_LIMIT, nothing else.
const key = base58(32), keyResults = await Promise.all(Array.from({ length: 8 }, () => grant(key, ip())))
const keyPaid = paid.filter(a => a === key).length
console.log(`one key, 8 at once: ${tally(keyResults)}; the net paid ${keyPaid} (limit 3)`)
ok &&= keyPaid === 3 && keyResults.filter(r => r === 'granted').length === 3 && keyResults.filter(r => r === 'FAUCET_LIMIT').length === 5

// One address, 15 keys at once: its limit is 10.
const from = ip(), before = paid.length
const ipResults = await Promise.all(Array.from({ length: 15 }, () => grant(base58(32), from)))
console.log(`one IP, 15 keys at once: ${tally(ipResults)}; the net paid ${paid.length - before} (limit 10)`)
ok &&= paid.length - before <= 10

// The route itself: 25 requests from one address in a minute, 20 answered.
const burst = ip(), burstResults = await Promise.all(Array.from({ length: 25 }, () => grant(base58(32), burst)))
const limited = burstResults.filter(r => r === 'RATE_LIMITED').length
console.log(`one IP, 25 requests: ${tally(burstResults)}`)
ok &&= limited === 5

api.kill()
stub.close()
console.log(ok ? 'PASS' : 'FAIL')
process.exit(ok ? 0 : 1)
