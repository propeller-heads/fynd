/**
 * Thin wrapper around openapi-fetch, bound to the fynd-rpc OpenAPI schema.
 *
 * The schema types live in `./schema.js`, which *is* auto-generated — re-generate it by running:
 *   cargo run -- openapi > clients/openapi.json
 *   openapi-typescript clients/openapi.json -o clients/typescript/client/src/schema.d.ts
 * This file is hand-maintained and is not touched by that pipeline.
 */

import createClient from "openapi-fetch";
import type { ClientOptions } from "openapi-fetch";
import type { paths } from "./schema.js";

export type { components, operations, paths } from "./schema.js";
export type { Middleware } from "openapi-fetch";

/** Optional transport settings for {@link createFyndClient}. */
export interface CreateFyndClientOptions {
  /** Headers sent with every request (e.g. `Authorization`). */
  headers?: Record<string, string>;
  /** Custom fetch implementation; defaults to `globalThis.fetch`. */
  fetch?: ClientOptions["fetch"];
}

/**
 * Create a typed fynd-rpc API client.
 *
 * @param baseUrl - Base URL of the fynd-rpc server (e.g. "http://localhost:8080")
 * @param options - Optional headers and fetch override
 * @returns A typed fetch client bound to the fynd-rpc OpenAPI schema
 */
export function createFyndClient(baseUrl: string, options?: CreateFyndClientOptions) {
  // exactOptionalPropertyTypes: only forward keys that are actually set.
  return createClient<paths>({
    baseUrl,
    ...(options?.headers !== undefined ? { headers: options.headers } : {}),
    ...(options?.fetch !== undefined ? { fetch: options.fetch } : {}),
  });
}

export type FyndClient = ReturnType<typeof createFyndClient>;
