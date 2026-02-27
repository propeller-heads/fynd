use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    BadRequest,
    NoRouteFound,
    InsufficientLiquidity,
    SolveTimeout,
    ServiceUnavailable,
    Unknown(String),
}

impl ErrorCode {
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

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::SolveTimeout | Self::ServiceUnavailable)
    }
}

#[derive(Debug, Error)]
pub enum FyndError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("provider error: {0}")]
    Provider(#[from] alloy::transports::RpcError<alloy::transports::TransportErrorKind>),

    #[error("API error ({code:?}): {message}")]
    Api { code: ErrorCode, message: String },

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("simulation failed: {0}")]
    SimulationFailed(String),

    #[error("configuration error: {0}")]
    Config(String),
}

impl FyndError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(_) => true,
            Self::Api { code, .. } => code.is_retryable(),
            _ => false,
        }
    }
}
