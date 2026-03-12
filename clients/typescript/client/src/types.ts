// Must stay in sync with components["schemas"]["QuoteStatus"] from @fynd/autogen.
export type SolutionStatus =
  | 'success'
  | 'no_route_found'
  | 'insufficient_liquidity'
  | 'timeout'
  | 'not_ready';

export type BackendKind = 'fynd' | 'turbine';

export type Address = `0x${string}`;
export type Hex = `0x${string}`;

export type OrderSide = 'sell';

export interface Order {
  tokenIn: Address;
  tokenOut: Address;
  amount: bigint;
  side: OrderSide;
  sender: Address;
  receiver?: Address;
}

export type UserTransferType = 'transfer_from' | 'transfer_from_permit2' | 'none';

export interface PermitDetails {
  token: Address;
  amount: bigint;
  expiration: bigint;
  nonce: bigint;
}

export interface PermitSingle {
  details: PermitDetails;
  spender: Address;
  sigDeadline: bigint;
}

export interface EncodingOptions {
  slippage: number;
  transferType?: UserTransferType;
  permit?: PermitSingle;
  permit2Signature?: Hex;
}

export interface Transaction {
  to: Address;
  value: bigint;
  data: Hex;
}

export interface QuoteOptions {
  timeoutMs?: number;
  minResponses?: number;
  maxGas?: bigint;
  encodingOptions?: EncodingOptions;
}

export interface QuoteParams {
  order: Order;
  options?: QuoteOptions;
}

export interface BlockInfo {
  number: number;
  hash: string;
  timestamp: number;
}

export interface Swap {
  poolId: string;       // wire: component_id
  protocol: string;
  tokenIn: Address;
  tokenOut: Address;
  amountIn: bigint;
  amountOut: bigint;
  gasEstimate: bigint;
}

export interface Route {
  swaps: Swap[];
}

export interface Quote {
  orderId: string;
  status: SolutionStatus;
  backend: BackendKind;
  route?: Route;
  amountIn: bigint;
  amountOut: bigint;
  gasEstimate: bigint;
  priceImpactBps?: number;
  block: BlockInfo;
  tokenOut: Address;    // from original Order; used by execute() for Transfer log parsing
  receiver: Address;    // from original Order; defaults to sender if Order.receiver was absent
  transaction?: Transaction;
}

export interface HealthStatus {
  healthy: boolean;
  lastUpdateMs: number;
  numSolverPools: number;
}
