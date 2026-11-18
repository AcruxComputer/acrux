// A JSON-RPC proxy that holds each call DELAY ms each way (145 ms each way = the 290 ms round trip
// measured to the public host); a new connection first pays two more round trips, its TCP and TLS
// handshakes, as it does against https://rpc.acrux.computer. Usage: node delay-proxy.mjs <target> <port>
import http from 'node:http'
const [target, port] = process.argv.slice(2)
const DELAY = Number(process.env.DELAY ?? 145)
const sleep = ms => new Promise(r => setTimeout(r, ms))
const server = http.createServer((req, res) => {
  let body = ''
  req.on('data', c => { body += c })
  req.on('end', async () => {
    if (req.socket.fresh) { req.socket.fresh = false; await sleep(4 * DELAY) }
    await sleep(DELAY)
    const answer = await fetch(target, { method: 'POST', headers: { 'content-type': 'application/json' }, body })
    const text = await answer.text()
    await sleep(DELAY)
    res.writeHead(answer.status, { 'content-type': 'application/json' }).end(text)
  })
})
server.on('connection', socket => { socket.fresh = true })
server.listen(Number(port), '127.0.0.1')
