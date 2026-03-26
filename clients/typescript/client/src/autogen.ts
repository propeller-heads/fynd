/**
 * Auto-generated TypeScript client for the fynd-rpc API.
 * Do not make direct changes to this file.
 * Re-generate by running: cargo run -- openapi > clients/openapi.json
 * then: openapi-typescript clients/openapi.json -o clients/typescript/client/src/schema.d.ts
 */

import createClient from "openapi-fetch";
import type { paths } from "./schema.js";

export type { components, operations, paths } from "./schema.js";

/**
 * Create a typed fynd-rpc API client.
 *
 * @param baseUrl - Base URL of the fynd-rpc server (e.g. "http://localhost:8080")
 * @returns A typed fetch client bound to the fynd-rpc OpenAPI schema
 */
export function createFyndClient(baseUrl: string) {
  return createClient<paths>({ baseUrl });
}

export type FyndClient = ReturnType<typeof createFyndClient>;
