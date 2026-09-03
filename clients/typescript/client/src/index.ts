export type {
  Address,
  ApprovalParams,
  BackendKind,
  BlockInfo,
  ClientFeeParams,
  EncodingOptions,
  FeeBreakdown,
  Hex,
  HealthStatus,
  InstanceInfo,
  Order,
  OrderSide,
  PermitDetails,
  PermitSingle,
  Quote,
  QuoteOptions,
  QuoteParams,
  Route,
  SimulationResult,
  SolutionStatus,
  Swap,
  Transaction,
  UserTransferType,
} from "./types.js";
export { FyndError } from "./error.js";
export type { ClientErrorCode, ErrorCode, ServerErrorCode } from "./error.js";
export type {
  ApprovalPayload,
  Eip1559Transaction,
  ExecutionReceipt,
  FyndPayload,
  PrimitiveSignature,
  SettledOrder,
  SettleOptions,
  SignedApproval,
  SignedSwap,
  SwapPayload,
  TxReceipt,
} from "./signing.js";
export { approvalSigningHash, assembleSignedSwap, DEFAULT_SETTLE_TIMEOUT_MS, swapSigningHash } from "./signing.js";
export {
  permit2SigningHash,
  encodingOptions,
  withPermit2,
  withVaultFunds,
} from "./permit2.js";
export {
  clientFeeSigningHash,
  patchClientFeeSignature,
  withClientFee,
} from "./client-fee.js";
export type { ClientFeeSwapContext } from "./client-fee.js";
export { FyndClient } from "./client.js";
export type {
  EthProvider,
  ExecutionOptions,
  FyndClientOptions,
  MinimalReceipt,
  RetryConfig,
  SigningHints,
} from "./client.js";
export { createFyndClient } from "./autogen.js";
export type { CreateFyndClientOptions, Middleware } from "./autogen.js";
export { viemProvider } from "./viem.js";
export type { ViemPublicClient } from "./viem.js";
