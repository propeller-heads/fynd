//! The HTTP API: `/v1/rfq/{venue}` serves a stream's current message and
//! `/v1/health` says whether any stream holds prices.

use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};

use actix_web::{
    dev::Server,
    error::ErrorNotFound,
    http::header::{ContentType, ETAG, IF_NONE_MATCH},
    web, App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use fynd_core::nexus::rfq::RfqVenue;
use serde_json::json;

use crate::{latest_stream::LatestStream, stream_kind::StreamKind};

/// The streams this service serves, by kind.
pub struct Streams {
    by_kind: HashMap<StreamKind, Arc<LatestStream>>,
}

impl Streams {
    /// A set of streams.
    pub fn new(streams: impl IntoIterator<Item = Arc<LatestStream>>) -> Self {
        Self {
            by_kind: streams
                .into_iter()
                .map(|stream| (stream.kind(), stream))
                .collect(),
        }
    }

    /// Every stream, in no particular order.
    pub fn all(&self) -> impl Iterator<Item = &Arc<LatestStream>> {
        self.by_kind.values()
    }

    fn find(&self, venue: &str) -> Result<Arc<LatestStream>, actix_web::Error> {
        let not_served = || ErrorNotFound(format!("no stream for venue '{venue}'"));
        let venue = RfqVenue::from_str(venue).map_err(|_| not_served())?;
        self.by_kind
            .get(&StreamKind::Rfq(venue))
            .cloned()
            .ok_or_else(not_served)
    }
}

/// Binds the HTTP API to `address` and returns the server to run, with the address it bound.
///
/// # Errors
///
/// Fails when `address` cannot be bound.
pub fn bind(streams: Arc<Streams>, address: SocketAddr) -> std::io::Result<(Server, SocketAddr)> {
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::from(streams.clone()))
            .route("/v1/health", web::get().to(health))
            .route("/v1/rfq/{venue}", web::get().to(current))
    })
    .bind(address)?;
    let bound = server
        .addrs()
        .first()
        .copied()
        .unwrap_or(address);
    Ok((server.run(), bound))
}

/// `GET /v1/health`: 200 while at least one stream holds prices, 503 otherwise.
async fn health(streams: web::Data<Streams>) -> impl Responder {
    let mut serving = Vec::new();
    let mut all = Vec::new();
    for stream in streams.all() {
        let name = stream.kind().to_string();
        if stream.is_serving() {
            serving.push(name.clone());
        }
        all.push(name);
    }
    serving.sort();
    all.sort();
    let body = json!({ "serving": serving, "streams": all });
    if serving.is_empty() {
        HttpResponse::ServiceUnavailable().json(body)
    } else {
        HttpResponse::Ok().json(body)
    }
}

/// `GET /v1/rfq/{venue}`: the stream's current message with its `ETag`.
///
/// Answers 304 with no body when `If-None-Match` equals the current `ETag`, and 204 while the
/// stream has had no snapshot in this run.
async fn current(
    request: HttpRequest,
    streams: web::Data<Streams>,
    venue: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let Some(message) = streams.find(&venue)?.current() else {
        return Ok(HttpResponse::NoContent().finish());
    };
    let etag = message.etag();
    let not_modified = request
        .headers()
        .get(IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes());
    if not_modified {
        return Ok(HttpResponse::NotModified()
            .insert_header((ETAG, etag))
            .finish());
    }
    Ok(HttpResponse::Ok()
        .insert_header((ETAG, etag))
        .insert_header(ContentType::json())
        .body(message.encoded))
}
