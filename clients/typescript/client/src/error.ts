import type { components } from "@fynd/autogen";

type WireErrorResponse = components["schemas"]["ErrorResponse"];

// Server-side codes — kept in sync with fynd-rpc/src/api/error.rs
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

export type ClientErrorCode = 'HTTP' | 'DESERIALIZE' | 'CONFIG' | 'SIMULATE_FAILED';
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

export class FyndError extends Error {
  readonly code: ErrorCode;
  readonly details?: unknown;

  constructor(message: string, code: ErrorCode, details?: unknown) {
    super(message);
    this.name = 'FyndError';
    this.code = code;
    if (details !== undefined) {
      this.details = details;
    }
  }

  isRetryable(): boolean {
    if (typeof this.code === 'string') {
      return RETRYABLE_CODES.has(this.code);
    }
    return false;
  }

  static fromWireError(wire: WireErrorResponse): FyndError {
    const code: ErrorCode = KNOWN_SERVER_CODES.has(wire.code)
      ? (wire.code as ServerErrorCode)
      : { kind: 'UNKNOWN', raw: wire.code };
    return new FyndError(wire.error, code, wire.details);
  }

  static config(message: string): FyndError {
    return new FyndError(message, 'CONFIG');
  }

  static simulateFailed(reason: string): FyndError {
    return new FyndError(reason, 'SIMULATE_FAILED');
  }
}
