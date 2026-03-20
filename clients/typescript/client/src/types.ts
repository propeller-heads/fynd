/** Outcome status of a quote request. Must stay in sync with the server schema. */
export type SolutionStatus =
  | 'success'
  | 'no_route_found'
  | 'insufficient_liquidity'
  | 'timeout'
  | 'not_ready';

/** Routing backend that produced a quote. */
export type BackendKind = 'fynd' | 'turbine';

/** EVM address as a hex string. */
export type Address = `0x${string}`;
/** Arbitrary hex-encoded bytes. */
export type Hex = `0x${string}`;

/** Order side. Currently only sell orders are supported. */
export type OrderSide = 'sell';

/** A swap order specifying input/output tokens, amount, and participants. */
export interface Order {
  /** Token to sell. */
  tokenIn: Address;
  /** Token to receive. */
  tokenOut: Address;
  /** Amount of `tokenIn` to sell (in token base units). */
  amount: bigint;
  side: OrderSide;
  /** Address that holds the input tokens and sends the transaction. */
  sender: Address;
  /** Address that receives output tokens. Defaults to `sender` if omitted. */
  receiver?: Address;
}

/** How the router pulls input tokens from the sender. */
export type UserTransferType = 'transfer_from' | 'transfer_from_permit2' | 'none';

/** Uniswap Permit2 allowance details for a single token. */
export interface PermitDetails {
  token: Address;
  /** Maximum transferable amount (uint160). */
  amount: bigint;
  /** Unix timestamp after which the permit expires (uint48). */
  expiration: bigint;
  /** Permit2 nonce for this token/spender pair (uint48). */
  nonce: bigint;
}

/** Uniswap Permit2 single-token permit, ready for EIP-712 signing. */
export interface PermitSingle {
  details: PermitDetails;
  /** Address authorized to spend tokens via Permit2. */
  spender: Address;
  /** Unix timestamp after which the signature is invalid. */
  sigDeadline: bigint;
}

/** Controls how the solver encodes the settlement transaction. */
export interface EncodingOptions {
  /** Maximum acceptable slippage as a fraction (e.g. 0.01 for 1%). */
  slippage: number;
  /** How tokens are transferred to the router. Defaults to `'transfer_from'`. */
  transferType?: UserTransferType;
  /** Permit2 permit data; required when `transferType` is `'transfer_from_permit2'`. */
  permit?: PermitSingle;
  /** 65-byte Permit2 signature over the permit; required with `permit`. */
  permit2Signature?: Hex;
}

/** An encoded on-chain transaction returned by the solver. */
export interface Transaction {
  to: Address;
  value: bigint;
  data: Hex;
}

/** Optional parameters for a quote request. */
export interface QuoteOptions {
  /** Server-side solver timeout in milliseconds. */
  timeoutMs?: number;
  /** Minimum number of solver responses to wait for before returning. */
  minResponses?: number;
  /** Maximum gas the solution may consume. */
  maxGas?: bigint;
  /** Encoding options; when set, the response includes a ready-to-sign transaction. */
  encodingOptions?: EncodingOptions;
}

/** Input parameters for {@link FyndClient.quote}. */
export interface QuoteParams {
  order: Order;
  options?: QuoteOptions;
}

/** Block metadata at the time the quote was computed. */
export interface BlockInfo {
  number: number;
  hash: string;
  /** Unix timestamp of the block (seconds). */
  timestamp: number;
}

/** A single pool-level swap within a route. */
export interface Swap {
  /** Unique pool identifier (wire name: `component_id`). */
  poolId: string;
  /** Protocol name (e.g. "uniswap_v3", "balancer_v2"). */
  protocol: string;
  tokenIn: Address;
  tokenOut: Address;
  amountIn: bigint;
  amountOut: bigint;
  gasEstimate: bigint;
}

/** An ordered sequence of swaps forming a complete routing path. */
export interface Route {
  swaps: Swap[];
}

/** A solver quote containing the best route, amounts, and optional encoded transaction. */
export interface Quote {
  orderId: string;
  status: SolutionStatus;
  backend: BackendKind;
  route?: Route;
  amountIn: bigint;
  amountOut: bigint;
  gasEstimate: bigint;
  /** Price impact in basis points (1 bp = 0.01%). */
  priceImpactBps?: number;
  block: BlockInfo;
  /** Output token address from the original order; used internally for settlement parsing. */
  tokenOut: Address;
  /** Receiver address; defaults to sender if not specified in the original order. */
  receiver: Address;
  /** Encoded transaction; present only when `encodingOptions` was set in the quote request. */
  transaction?: Transaction;
}

/** Solver health status and readiness information. */
export interface HealthStatus {
  healthy: boolean;
  /** Milliseconds since the last state update. */
  lastUpdateMs: number;
  /** Number of liquidity pools tracked by the solver. */
  numSolverPools: number;
  gasPriceAgeMs?: number;
}
