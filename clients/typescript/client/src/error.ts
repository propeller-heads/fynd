/** Error thrown by FyndClient operations. */
export class FyndClientError extends Error {
  constructor(
    message: string,
    /** Whether the operation can be retried safely. */
    public readonly retryable: boolean,
    /** The HTTP status code, if applicable. */
    public readonly statusCode?: number,
  ) {
    super(message);
    this.name = "FyndClientError";
  }
}

export function isRetryable(error: unknown): boolean {
  if (error instanceof FyndClientError) {
    return error.retryable;
  }
  // Network errors (fetch failures) are generally retryable
  if (error instanceof TypeError) {
    return true;
  }
  return false;
}
