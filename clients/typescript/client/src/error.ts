import type { components } from "./autogen.js";

type WireErrorResponse = components["schemas"]["ErrorResponse"];

/** Error code returned by the Fynd server. Kept in sync with the server schema. */
export type ServerErrorCode =
  | 'BAD_REQUEST'
  | 'NO_ROUTE_FOUND'
  | 'INSUFFICIENT_LIQUIDITY'
  | 'TIMEOUT'
  | 'QUEUE_FULL'
  | 'SERVICE_OVERLOADED'
  | 'STALE_DATA'
  | 'INVALID_ORDER'
  | 'INTERNAL_ERROR'
  | 'NOT_READY'
  | 'ALGORITHM_ERROR'
  | { kind: 'UNKNOWN'; raw: string };

/** Error code originating from the client SDK itself. */
export type ClientErrorCode = 'HTTP' | 'DESERIALIZE' | 'CONFIG' | 'SIMULATE_FAILED' | 'SETTLE_TIMEOUT' | 'EXECUTION_REVERTED';

/** Union of all error codes, covering both server and client errors. */
export type ErrorCode = ServerErrorCode | ClientErrorCode;

const KNOWN_SERVER_CODES = new Set([
  'BAD_REQUEST',
  'NO_ROUTE_FOUND',
  'INSUFFICIENT_LIQUIDITY',
  'TIMEOUT',
  'QUEUE_FULL',
  'SERVICE_OVERLOADED',
  'STALE_DATA',
  'INVALID_ORDER',
  'INTERNAL_ERROR',
  'NOT_READY',
  'ALGORITHM_ERROR',
]);

const RETRYABLE_CODES = new Set<string>([
  'TIMEOUT',
  'QUEUE_FULL',
  'SERVICE_OVERLOADED',
  'STALE_DATA',
  'NOT_READY',
  'HTTP',
]);

/**
 * Typed error thrown by all Fynd client operations.
 *
 * Use {@link FyndError.code} to programmatically handle specific failure modes,
 * and {@link FyndError.isRetryable} to decide whether to retry.
 */
export class FyndError extends Error {
  /** Machine-readable error code identifying the failure category. */
  readonly code: ErrorCode;
  /** Optional structured details from the server error response. */
  readonly details?: unknown;

  constructor(message: string, code: ErrorCode, details?: unknown) {
    super(message);
    this.name = 'FyndError';
    this.code = code;
    if (details !== undefined) {
      this.details = details;
    }
  }

  /** Returns `true` if the error is transient and the operation can be retried. */
  isRetryable(): boolean {
    if (typeof this.code === 'string') {
      return RETRYABLE_CODES.has(this.code);
    }
    return false;
  }

  /** Creates a `FyndError` from a server error response or unstructured error. */
  static fromWireError(wire: WireErrorResponse | unknown): FyndError {
    const obj = wire as Record<string, unknown>;
    const rawCode = obj['code'];
    const rawError = obj['error'];
    if (typeof rawCode !== 'string' || typeof rawError !== 'string') {
      const message = typeof wire === 'string' ? wire : JSON.stringify(wire);
      return new FyndError(message, 'HTTP');
    }
    const code: ErrorCode = KNOWN_SERVER_CODES.has(rawCode)
      ? (rawCode as ServerErrorCode)
      : { kind: 'UNKNOWN', raw: rawCode };
    return new FyndError(rawError, code, obj['details']);
  }

  /** Creates a `CONFIG` error for invalid or missing client configuration. */
  static config(message: string): FyndError {
    return new FyndError(message, 'CONFIG');
  }

  /** Creates a `SIMULATE_FAILED` error when an on-chain simulation reverts. */
  static simulateFailed(reason: string): FyndError {
    return new FyndError(reason, 'SIMULATE_FAILED');
  }

  /** Creates a `SETTLE_TIMEOUT` error when transaction confirmation takes too long. */
  static timeout(message: string): FyndError {
    return new FyndError(message, 'SETTLE_TIMEOUT');
  }

  /** Creates an `EXECUTION_REVERTED` error when a mined transaction reverts. */
  static executionReverted(reason: string): FyndError {
    return new FyndError(reason, 'EXECUTION_REVERTED');
  }
}
