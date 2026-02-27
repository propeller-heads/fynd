/** A swap order to get a quote for. */
export interface Order {
  tokenIn: string; // hex address, e.g. "0xC02aaa..."
  tokenOut: string;
  /** Amount in token units (as bigint). */
  amount: bigint;
  side: "sell";
  sender: string;
  receiver?: string;
}

/** A priced route returned by the solver, bound to a specific block. */
export interface OrderSolution {
  orderId: string;
  amountIn: bigint;
  amountOut: bigint;
  gasEstimate: bigint;
  priceImpactBps?: number;
  block: BlockInfo;
  route?: Route;
}

export interface BlockInfo {
  number: number;
  hash: string;
  timestamp: number;
}

export interface Route {
  swaps: Swap[];
}

export interface Swap {
  componentId: string;
  protocol: string;
  tokenIn: string;
  tokenOut: string;
  amountIn: bigint;
  amountOut: bigint;
  gasEstimate: bigint;
}
