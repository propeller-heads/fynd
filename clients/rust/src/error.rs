use thiserror::Error;

/// A structured error code returned by the Fynd RPC API.
///
/// Mapped from the raw string `code` field in
/// [`fynd_rpc_types::ErrorResponse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    /// The request was malformed or contained invalid parameters.
    ///
    /// Server codes: `BAD_REQUEST`, `INVALID_ORDER`.
    BadRequest,

    /// No swap route exists between the requested token pair.
    ///
    /// Server code: `NO_ROUTE_FOUND`.
    NoRouteFound,

    /// A route exists but available pool liquidity is too shallow for the requested amount.
    ///
    /// Server code: `INSUFFICIENT_LIQUIDITY`.
    InsufficientLiquidity,

    /// The solver timed out before returning a route. Retrying may succeed.
    ///
    /// Server code: `TIMEOUT`.
    SolveTimeout,

    /// The server is temporarily unavailable (overloaded, queue full, stale data, or not yet
    /// initialised). Retrying after a short backoff should succeed.
    ///
    /// Server codes: `QUEUE_FULL`, `SERVICE_OVERLOADED`, `STALE_DATA`, `NOT_READY`.
    ServiceUnavailable,

    /// An unrecognised server error code. The raw string is preserved for debugging.
    Unknown(String),
}

impl ErrorCode {
    /// Map a raw server error code string to a typed [`ErrorCode`].
    ///
    /// Unknown codes are wrapped in [`ErrorCode::Unknown`] rather than panicking.
    pub fn from_server_code(code: &str) -> Self {
        match code {
            "BAD_REQUEST" | "INVALID_ORDER" => Self::BadRequest,
            "NO_ROUTE_FOUND" => Self::NoRouteFound,
            "INSUFFICIENT_LIQUIDITY" => Self::InsufficientLiquidity,
            "TIMEOUT" => Self::SolveTimeout,
            "QUEUE_FULL" | "SERVICE_OVERLOADED" | "STALE_DATA" | "NOT_READY" => {
                Self::ServiceUnavailable
            }
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Returns `true` if this error code indicates that the request is safe to retry.
    ///
    /// Only [`SolveTimeout`](Self::SolveTimeout) and
    /// [`ServiceUnavailable`](Self::ServiceUnavailable) are retryable; all other codes
    /// represent permanent failures.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::SolveTimeout | Self::ServiceUnavailable)
    }
}

/// Errors that can be returned by [`FyndClient`](crate::FyndClient) methods.
#[derive(Debug, Error)]
pub enum FyndError {
    /// An HTTP-level error from the underlying `reqwest` client (network failure, timeout, etc.).
    ///
    /// HTTP errors are always considered retryable.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// An error returned by the Ethereum JSON-RPC provider (e.g. during nonce/fee estimation or
    /// transaction submission).
    #[error("provider error: {0}")]
    Provider(#[from] alloy::transports::RpcError<alloy::transports::TransportErrorKind>),

    /// A structured error response from the Fynd RPC API. Check `code` to distinguish permanent
    /// failures (e.g. `NoRouteFound`) from transient ones (e.g. `SolveTimeout`).
    #[error("API error ({code:?}): {message}")]
    Api { code: ErrorCode, message: String },

    /// Malformed or unexpected data in the API response (e.g. an address with the wrong byte
    /// length, an unrecognised enum variant).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A `eth_call` simulation of the swap transaction reverted. The message contains the
    /// revert reason when available.
    #[error("simulation failed: {0}")]
    SimulationFailed(String),

    /// Invalid client configuration (e.g. unparseable URL, missing sender address).
    #[error("configuration error: {0}")]
    Config(String),
}

impl FyndError {
    /// Returns `true` if the operation that produced this error can safely be retried.
    ///
    /// HTTP errors and certain API error codes are retryable. Protocol, config, and simulation
    /// errors are not.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(_) => true,
            Self::Api { code, .. } => code.is_retryable(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_from_known_server_codes() {
        assert_eq!(ErrorCode::from_server_code("BAD_REQUEST"), ErrorCode::BadRequest);
        assert_eq!(ErrorCode::from_server_code("NO_ROUTE_FOUND"), ErrorCode::NoRouteFound);
        assert_eq!(
            ErrorCode::from_server_code("INSUFFICIENT_LIQUIDITY"),
            ErrorCode::InsufficientLiquidity
        );
        assert_eq!(ErrorCode::from_server_code("INVALID_ORDER"), ErrorCode::BadRequest);
        assert_eq!(ErrorCode::from_server_code("TIMEOUT"), ErrorCode::SolveTimeout);
        assert_eq!(ErrorCode::from_server_code("QUEUE_FULL"), ErrorCode::ServiceUnavailable);
        assert_eq!(
            ErrorCode::from_server_code("SERVICE_OVERLOADED"),
            ErrorCode::ServiceUnavailable
        );
        assert_eq!(ErrorCode::from_server_code("STALE_DATA"), ErrorCode::ServiceUnavailable);
        assert_eq!(ErrorCode::from_server_code("NOT_READY"), ErrorCode::ServiceUnavailable);
    }

    #[test]
    fn error_code_unknown_for_unrecognised_codes() {
        assert!(matches!(ErrorCode::from_server_code("ALGORITHM_ERROR"), ErrorCode::Unknown(_)));
        assert!(matches!(ErrorCode::from_server_code("INTERNAL_ERROR"), ErrorCode::Unknown(_)));
        assert!(matches!(ErrorCode::from_server_code("WHATEVER"), ErrorCode::Unknown(_)));
    }

    #[test]
    fn is_retryable_true_for_retryable_codes() {
        assert!(
            FyndError::Api { code: ErrorCode::SolveTimeout, message: String::new() }.is_retryable()
        );
        assert!(FyndError::Api { code: ErrorCode::ServiceUnavailable, message: String::new() }
            .is_retryable());
    }

    #[test]
    fn is_retryable_false_for_non_retryable_errors() {
        assert!(
            !FyndError::Api { code: ErrorCode::BadRequest, message: String::new() }.is_retryable()
        );
        assert!(!FyndError::Api { code: ErrorCode::NoRouteFound, message: String::new() }
            .is_retryable());
        assert!(!FyndError::Protocol("bad data".into()).is_retryable());
        assert!(!FyndError::Config("missing sender".into()).is_retryable());
    }
}
