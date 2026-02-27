/**
 * The payload the caller must sign to execute a trade.
 *
 * Call `signingHash` to get the bytes32 hash to sign externally,
 * then pass the hex signature to `assembleSignedOrder()`.
 */
export type SignablePayload =
  | { backend: "fynd"; signingHash: string; rawTx: FyndRawTx }
  | { backend: "turbine" }; // placeholder, not yet implemented

export interface FyndRawTx {
  to: string;
  data: string;
  value: string;
  gasLimit: bigint;
  maxFeePerGas: bigint;
  maxPriorityFeePerGas: bigint;
  nonce: number;
  chainId: number;
}

export interface SignedOrder {
  backend: "fynd" | "turbine";
  /** Signed EIP-1559 transaction as hex RLP, ready for eth_sendRawTransaction. */
  rawTx: string;
}

/**
 * Combines a signable payload with a caller-supplied hex signature.
 *
 * The caller signs `payload.signingHash` externally (hardware wallet, KMS, etc.)
 * and passes the resulting hex signature here.
 *
 * Note: This function does not encode the full RLP transaction — actual RLP
 * encoding requires an Ethereum library (ethers.js, viem, etc.) on the caller's
 * side. This function validates inputs and returns the signed order stub.
 * In a future version this will accept a pre-encoded raw transaction.
 */
export function assembleSignedOrder(
  payload: SignablePayload,
  signedRawTx: string,
): SignedOrder {
  if (payload.backend === "turbine") {
    throw new Error("Turbine signing not yet implemented");
  }
  return {
    backend: "fynd",
    rawTx: signedRawTx,
  };
}
