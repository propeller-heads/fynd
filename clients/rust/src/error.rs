use thiserror::Error;

#[derive(Debug, Error)]
pub enum FyndClientError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no route found for order {order_id}")]
    NoRouteFound { order_id: String },
    #[error("insufficient liquidity for order {order_id}")]
    InsufficientLiquidity { order_id: String },
    #[error("solver timeout for order {order_id}")]
    Timeout { order_id: String },
    #[error("solver not ready")]
    NotReady,
    #[error("RPC error: {0}")]
    Rpc(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
}
