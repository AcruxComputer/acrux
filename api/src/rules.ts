// The rule changes the validator set voted on, as the net's `getRules` reads them from state.
import { Rules } from '@acrux/contract'

export async function readRules(rpcUrl: string): Promise<Rules> {
  const response = await fetch(rpcUrl, { method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getRules', params: {} }), signal: AbortSignal.timeout(5000) })
  const body = await response.json() as { result?: unknown; error?: unknown }
  if (body.error) throw new Error('getRules failed')
  return Rules.parse(body.result)
}
