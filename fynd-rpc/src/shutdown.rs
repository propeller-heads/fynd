//! Graceful shutdown for the HTTP server and the long-lived responses it serves.
//!
//! Actix waits for in-flight responses when the server stops, up to its shutdown timeout. A
//! stream that never ends on its own (an embedder's server-sent events route, say) would hold
//! every stop for that full timeout, so
//! [`ShutdownHandle::stop`](crate::shutdown::ShutdownHandle::stop)
//! fires the [`ShutdownSignal`](crate::shutdown::ShutdownSignal) first and such a response ends
//! itself.

use actix_web::dev::ServerHandle;
use tokio::sync::watch;

/// Fires once when the server begins to shut down.
///
/// Every clone observes the same signal. A handler serving a long-lived response reads it from
/// [`AppState::shutdown_signal`](crate::api::AppState::shutdown_signal) and ends the response
/// when [`Self::triggered`] resolves.
#[derive(Debug, Clone)]
pub struct ShutdownSignal(watch::Sender<bool>);

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl ShutdownSignal {
    /// Creates a signal that has not fired.
    #[must_use]
    pub fn new() -> Self {
        Self(watch::Sender::new(false))
    }

    /// Fires the signal. Firing it again changes nothing.
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }

    /// Whether the signal has fired.
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the signal has fired, at once when it already has.
    pub async fn triggered(&self) {
        let mut fired = self.0.subscribe();
        // `self` holds the sender, so the wait ends only by the flag being set.
        let _set = fired.wait_for(|fired| *fired).await;
    }
}

/// Stops a running server gracefully, ending long-lived responses first.
#[derive(Debug, Clone)]
pub struct ShutdownHandle {
    server: ServerHandle,
    signal: ShutdownSignal,
}

impl ShutdownHandle {
    pub(crate) fn new(server: ServerHandle, signal: ShutdownSignal) -> Self {
        Self { server, signal }
    }

    /// Fires the [`ShutdownSignal`], then stops the HTTP server and waits for in-flight requests.
    pub async fn stop(&self) {
        self.signal.trigger();
        self.server.stop(true).await;
    }

    /// Returns the server's own handle, which stops it without firing the signal.
    pub(crate) fn server(&self) -> &ServerHandle {
        &self.server
    }

    /// Returns the signal [`Self::stop`] fires.
    pub(crate) fn signal(&self) -> &ShutdownSignal {
        &self.signal
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn test_shutdown_signal() {
        let signal = ShutdownSignal::new();
        let observer = signal.clone();
        assert!(!observer.is_triggered());
        let waiting = tokio::spawn(async move { observer.triggered().await });

        signal.trigger();

        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("triggered resolves once fired")
            .expect("the waiting task does not panic");
        assert!(signal.is_triggered());
        tokio::time::timeout(Duration::from_secs(1), signal.triggered())
            .await
            .expect("an already fired signal resolves at once");
    }
    /// A stream that ends on the signal lets the stop finish long before actix's shutdown
    /// timeout, which it would otherwise wait out for a client that never disconnects.
    #[actix_web::test]
    async fn test_stop_ends_streams_before_the_shutdown_timeout() {
        use actix_web::{web, App, HttpResponse, HttpServer};

        let signal = ShutdownSignal::new();
        let route_signal = signal.clone();
        let server = HttpServer::new(move || {
            let route_signal = route_signal.clone();
            App::new().route(
                "/stream",
                web::get().to(move || {
                    let route_signal = route_signal.clone();
                    async move {
                        let body = futures::stream::once(async move {
                            route_signal.triggered().await;
                            Ok::<_, actix_web::Error>(web::Bytes::from_static(b"bye"))
                        });
                        HttpResponse::Ok().streaming(body)
                    }
                }),
            )
        })
        .workers(1)
        .shutdown_timeout(30)
        // An idle keep-alive connection holds the stop for its own timeout once the stream has
        // ended; that is actix's behaviour for any request, not what this test is about.
        .keep_alive(actix_web::http::KeepAlive::Disabled)
        .bind(("127.0.0.1", 0))
        .expect("bind an ephemeral port");
        let address = server.addrs()[0];
        let server = server.run();
        let shutdown = ShutdownHandle::new(server.handle(), signal);
        let server_task = tokio::spawn(server);
        let response = reqwest::get(format!("http://{address}/stream"))
            .await
            .expect("the stream opens");

        tokio::time::timeout(Duration::from_secs(5), shutdown.stop())
            .await
            .expect("the stop does not wait for the open stream");

        assert_eq!(
            response
                .text()
                .await
                .expect("the stream ends"),
            "bye"
        );
        server_task
            .await
            .expect("the server task does not panic")
            .expect("the server stops cleanly");
    }
}
