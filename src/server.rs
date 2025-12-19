use std::{convert::Infallible, sync::Arc};

use serde_json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use warp::{Filter, Reply};

use crate::{
    api::RouterApi,
    models::{Order, SolverError},
    modules::algorithm::algorithm::Algorithm,
};

/// HTTP server that exposes the RouterApi endpoints
///
/// This is much simpler than the complex locking approach since the solver
/// manages its own background updates internally with Arc<Mutex<>>
pub struct RouterServer<A: Algorithm + Send + 'static> {
    api: Arc<Mutex<RouterApi<A>>>,
    port: u16,
}

impl<A: Algorithm + Send + Sync + 'static> RouterServer<A> {
    pub fn new(api: RouterApi<A>, port: u16) -> Self {
        Self { api: Arc::new(Mutex::new(api)), port }
    }

    /// Start the HTTP server
    ///
    /// Much simpler than before since:
    /// 1. Solver handles its own background updates
    /// 2. No complex shared state management
    /// 3. RouterApi can be used normally
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("Starting Router Server on port {}", self.port);

        // Create cancellation token for graceful shutdown
        let shutdown_token = CancellationToken::new();
        let server_shutdown = shutdown_token.clone();

        // Create HTTP routes
        let routes = self.create_routes();

        // Start HTTP server task
        let server_handle = tokio::spawn(async move {
            let server = warp::serve(routes).run(([0, 0, 0, 0], self.port));

            tokio::select! {
                _ = server_shutdown.cancelled() => {
                    println!("HTTP server shutting down gracefully...");
                }
                _ = server => {
                    println!("HTTP server completed");
                }
            }
        });

        // Handle graceful shutdown
        let shutdown_handle = tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let sigterm = signal(SignalKind::terminate()).ok();

                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        println!("Received Ctrl+C, shutting down...");
                    }
                    _ = async {
                        if let Some(mut sigterm) = sigterm {
                            sigterm.recv().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {
                        println!("Received SIGTERM, shutting down...");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        println!("Received Ctrl+C, shutting down...");
                    }
                }
            }

            shutdown_token.cancel();
        });

        // Wait for server or shutdown signal
        tokio::select! {
            result = server_handle => {
                println!("Server ended: {:?}", result);
            }
            _ = shutdown_handle => {
                println!("Shutdown signal received");
            }
        }

        println!("Router server shutdown complete");
        Ok(())
    }

    /// Create HTTP routes - clean and simple
    fn create_routes(&self) -> impl Filter<Extract = impl Reply> + Clone {
        let api = Arc::clone(&self.api);

        // POST /solve - Get routes and encoded transactions (no execution)
        let solve_api = Arc::clone(&api);
        let solve = warp::path("solve")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(move |orders: Vec<Order>| {
                let api = Arc::clone(&solve_api);
                async move {
                    let api_guard = api.lock().await;
                    match api_guard.solve(&orders).await {
                        Ok(response) => Ok(warp::reply::json(&response)),
                        Err(e) => {
                            eprintln!("Solve error: {}", e);
                            Err(warp::reject::custom(ApiError::from(e)))
                        }
                    }
                }
            });

        // POST /solve_and_execute - Solve, encode, and execute transactions
        let solve_execute_api = Arc::clone(&api);
        let solve_and_execute = warp::path("solve_and_execute")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(move |orders: Vec<Order>| {
                let api = Arc::clone(&solve_execute_api);
                async move {
                    let mut api_guard = api.lock().await;
                    match api_guard
                        .solve_and_execute(&orders)
                        .await
                    {
                        Ok(response) => Ok(warp::reply::json(&response)),
                        Err(e) => {
                            eprintln!("Solve and execute error: {}", e);
                            Err(warp::reject::custom(ApiError::from(e)))
                        }
                    }
                }
            });

        // POST /track - Track transaction status
        let track_api = Arc::clone(&api);
        let track = warp::path("track_tx")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(move |tx_hashes: Vec<String>| {
                let api = Arc::clone(&track_api);
                async move {
                    let api_guard = api.lock().await;
                    match api_guard
                        .track_transactions(&tx_hashes)
                        .await
                    {
                        Ok(response) => Ok(warp::reply::json(&response)),
                        Err(e) => {
                            eprintln!("Track transactions error: {}", e);
                            Err(warp::reject::custom(ApiError::from(e)))
                        }
                    }
                }
            });

        // GET /health - Health check
        let health = warp::path("health")
            .and(warp::get())
            .and_then(health_handler);

        solve
            .or(solve_and_execute)
            .or(track)
            .or(health)
            .with(warp::cors().allow_any_origin())
            .recover(handle_rejection)
    }
}

/// Health check handler
async fn health_handler() -> Result<impl Reply, Infallible> {
    Ok(warp::reply::json(&serde_json::json!({
        "status": "healthy",
        "service": "tycho-router",
        "version": env!("CARGO_PKG_VERSION")
    })))
}

/// Custom error type for API responses
#[derive(Debug)]
pub struct ApiError {
    message: String,
}

impl warp::reject::Reject for ApiError {}

impl From<SolverError> for ApiError {
    fn from(err: SolverError) -> Self {
        Self { message: err.to_string() }
    }
}

/// Handle API errors and convert to HTTP responses
async fn handle_rejection(err: warp::Rejection) -> Result<impl Reply, Infallible> {
    if let Some(api_err) = err.find::<ApiError>() {
        let json = warp::reply::json(&serde_json::json!({
            "error": api_err.message
        }));
        Ok(warp::reply::with_status(json, warp::http::StatusCode::BAD_REQUEST))
    } else if err.is_not_found() {
        let json = warp::reply::json(&serde_json::json!({
            "error": "Route not found"
        }));
        Ok(warp::reply::with_status(json, warp::http::StatusCode::NOT_FOUND))
    } else if let Some(_) = err.find::<warp::filters::body::BodyDeserializeError>() {
        let json = warp::reply::json(&serde_json::json!({
            "error": "Invalid JSON body"
        }));
        Ok(warp::reply::with_status(json, warp::http::StatusCode::BAD_REQUEST))
    } else {
        eprintln!("Unhandled rejection: {:?}", err);
        let json = warp::reply::json(&serde_json::json!({
            "error": "Internal server error"
        }));
        Ok(warp::reply::with_status(json, warp::http::StatusCode::INTERNAL_SERVER_ERROR))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: Add integration tests for RouterServer
    // - Test quote endpoint with real RouterApi
    // - Test solve endpoint with real RouterApi
    // - Test health endpoint
    // - Test error handling for invalid requests
    // - Test graceful shutdown behavior
}
