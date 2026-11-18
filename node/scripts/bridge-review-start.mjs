// Round 2 named checks: reject published identities before binding ports; wire host deposits.
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const backend = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const nodeDir = join(backend, 'node')
const dir = mkdtempSync(join(nodeDir, 'target/bridge-review-start-'))
const rule = join(dir, 'bridge.json')
const idl = JSON.parse(readFileSync(join(backend, 'chain/target/idl/acrux_bridge.json'), 'utf8'))
writeFileSync(rule, JSON.stringify({ program: idl.address, fromSlot: 0, chainId: 'acrux-startup-check' }))
try {
  for (const seed of [undefined, '', '20261116']) {
    const env = { ...process.env, ACRUX_BRIDGE: rule, ACRUX_BRIDGE_PROOF_MAX: '100000', ACRUX_BRIDGE_RPC: 'http://127.0.0.1:5041,http://127.0.0.1:5042' }
    if (seed === undefined) delete env.ACRUX_IDENTITY_SEED
    else env.ACRUX_IDENTITY_SEED = seed
    const result = spawnSync(join(nodeDir, 'target/release', process.platform === 'win32' ? 'acrux.exe' : 'acrux'), [
      'net', 'up', '--genesis', join(backend, 'chain/genesis/acrux-net.json'), '--store', join(dir, 'net'),
      '--gateway-port', '5043', '--ws-port', '5044', '--metrics-port', '5045', '--base-port', '5049',
    ], { cwd: nodeDir, env, encoding: 'utf8', timeout: 5000, windowsHide: true })
    assert.notEqual(result.status, 0)
    assert.match(result.stderr, /ACRUX_IDENTITY_SEED must be set to a private non-development seed/, 'bridge startup must explain why missing/published identities are refused')
  }
  console.log('PASS live identities: actual bridge startup rejects missing, empty and published seeds')
  const compose = spawnSync('docker', ['compose', '--env-file', 'deploy/host/acrux.env.example', '-f', 'deploy/host/compose.yml', 'config', '--no-env-resolution', '--format', 'json'], { cwd: backend, encoding: 'utf8', windowsHide: true })
  assert.equal(compose.status, 0, 'Compose configuration must parse')
  const net = JSON.parse(compose.stdout).services.net
  assert.ok(Object.hasOwn(net.environment, 'ACRUX_BRIDGE'), 'net must receive the bridge rule path')
  assert.ok(Object.hasOwn(net.environment, 'ACRUX_BRIDGE_RPC'), 'net must receive both deposit providers')
  assert.equal(String(net.environment.ACRUX_BRIDGE_PROOF_MAX), '100000')
  assert.ok(net.volumes.some(volume => volume.target === '/etc/acrux/bridge.json' && volume.read_only), 'only the rule is mounted into net, read-only')
  console.log('PASS host deposits: rule, both-provider setting, archive capacity and rule mount reach net')
} finally {
  rmSync(dir, { recursive: true, force: true })
}
