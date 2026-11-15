import { ApiClientError, type ErrorCode } from '@acrux/contract'

/** Every failure, from the gateway or from the SDK itself, is the contract's error: `code`, `status`, `body.error`. */
export { ApiClientError as AcruxError }
export type { ErrorCode }

export const invalid = (message: string) =>
  new ApiClientError(400, { error: { code: 'INVALID_INPUT', message, retryable: false, stage: 'validation' } })

export const unavailable = (message: string) =>
  new ApiClientError(503, { error: { code: 'NET_UNAVAILABLE', message, retryable: true, stage: 'read' } })
