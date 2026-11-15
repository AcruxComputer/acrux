import { ApiError } from './schemas.js'
import { ApiClientError } from './http.js'
import { ProtocolEvent, StreamCommand, type SubscribeFilter } from './events.js'
import { rpcMethods, type RpcMethod, type RpcParams, type RpcResult } from './rpc.js'

export type GatewayOptions = { rpcUrl: string; wsUrl: string; fetch?: typeof globalThis.fetch; WebSocket?: typeof globalThis.WebSocket; fixture?: string }
export function createGatewayClient(options: GatewayOptions) {
  const fetcher = options.fetch ?? globalThis.fetch.bind(globalThis)
  let nextId = 0
  return {
    async call<M extends RpcMethod>(method: M, params: RpcParams<M>, signal?: AbortSignal): Promise<RpcResult<M>> {
      const id = ++nextId
      let response: Response
      try { response = await fetcher(options.rpcUrl, { method: 'POST', signal,
        headers: { 'Content-Type': 'application/json', ...(options.fixture ? { 'X-Fixture': options.fixture } : {}) },
        body: JSON.stringify({ jsonrpc: '2.0', id, method, params: rpcMethods[method].params.parse(params) }),
      }) } catch { throw new ApiClientError(503, { error: { code: 'NET_UNAVAILABLE', message: 'The gateway could not be reached.', retryable: true, stage: 'read' } }) }
      const body = await response.json() as { jsonrpc?: string; id?: unknown; result?: unknown; error?: { code?: number; message?: string; data?: unknown } }
      if (body.error) {
        const parsed = ApiError.safeParse(body.error.data)
        throw new ApiClientError(response.status, parsed.success ? parsed.data : { error: { code: 'NET_UNAVAILABLE', message: body.error.message ?? 'Gateway failed.', retryable: true, stage: 'read' } })
      }
      if (!response.ok || body.jsonrpc !== '2.0' || body.id !== id) throw new ApiClientError(response.status, { error: { code: 'NET_UNAVAILABLE', message: 'Gateway response did not match the request.', retryable: true, stage: 'read' } })
      return rpcMethods[method].result.parse(body.result) as RpcResult<M>
    },
    subscribe(filter: SubscribeFilter, onEvent: (event: ProtocolEvent) => void, handlers: { onOpen?: () => void; onClose?: (event: CloseEvent) => void; onError?: (error: unknown) => void; afterSequence?: number; streamId?: string } = {}) {
      const Socket = options.WebSocket ?? globalThis.WebSocket
      const url = new URL(options.wsUrl)
      if (options.fixture) url.searchParams.set('fixture', options.fixture)
      const socket = new Socket(url)
      socket.onopen = () => {
        socket.send(JSON.stringify(StreamCommand.parse({ version: 1, type: 'subscribe', filter, afterSequence: handlers.afterSequence, streamId: handlers.streamId })))
        handlers.onOpen?.()
      }
      socket.onmessage = ({ data }) => {
        try { onEvent(ProtocolEvent.parse(JSON.parse(String(data)))) } catch (error) { handlers.onError?.(error) }
      }
      socket.onerror = (error) => handlers.onError?.(error)
      socket.onclose = (event) => handlers.onClose?.(event)
      return { socket, close: () => socket.close(1000, 'Client unsubscribed') }
    },
  }
}