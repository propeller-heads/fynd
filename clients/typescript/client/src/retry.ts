import { isRetryable } from "./error.js";

export interface RetryOptions {
  maxAttempts: number;
  baseDelayMs: number;
  maxDelayMs: number;
}

export const DEFAULT_RETRY_OPTIONS: RetryOptions = {
  maxAttempts: 3,
  baseDelayMs: 200,
  maxDelayMs: 5000,
};

/**
 * Retry an async operation with exponential backoff and jitter.
 * Only retries on retryable errors.
 */
export async function withRetry<T>(
  fn: () => Promise<T>,
  options: RetryOptions = DEFAULT_RETRY_OPTIONS,
): Promise<T> {
  for (let attempt = 0; attempt < options.maxAttempts; attempt++) {
    try {
      return await fn();
    } catch (error) {
      if (!isRetryable(error) || attempt === options.maxAttempts - 1) {
        throw error;
      }

      // Exponential backoff with full jitter
      const baseDelay = Math.min(
        options.baseDelayMs * Math.pow(2, attempt),
        options.maxDelayMs,
      );
      const jitter = Math.random() * baseDelay;
      await new Promise((resolve) => setTimeout(resolve, jitter));
    }
  }

  // Unreachable: the loop always throws on the final attempt.
  // This satisfies the TypeScript return-type checker.
  throw new Error("withRetry: maxAttempts must be >= 1");
}
