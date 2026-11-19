//! The HTTP API: `/v1/price-levels/{venue}` serves a stream's current message,
//! `/v1/derived/{kind}` serves the latest snapshot of a derived computation and `/v1/health` says
//! whether the service holds data.

use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};

use actix_web::{
    dev::Server,
    error::ErrorNotFound,
    http::header::{
        AcceptEncoding, ContentEncoding, ContentType, Encoding, Preference, Quality, ETAG,
        IF_NONE_MATCH, VARY,
    },
    web, App, HttpMessage as _, HttpRequest, HttpResponse, HttpServer, Responder,
};
use fynd_core::nexus::{derived::DerivedKind, price_levels::PriceLevelVenue};
use serde_json::json;

use crate::{derived::snapshot::DerivedSnapshots, latest_prices::LatestPrices};

/// The derived snapshots this service serves, or `None` when it does not run the market.
type DerivedStore = Option<Arc<DerivedSnapshots>>;

/// The streams this service serves, by venue.
pub struct Streams {
    by_venue: HashMap<PriceLevelVenue, Arc<LatestPrices>>,
}

impl Streams {
    /// A set of streams.
    pub fn new(streams: impl IntoIterator<Item = Arc<LatestPrices>>) -> Self {
        Self {
            by_venue: streams
                .into_iter()
                .map(|stream| (stream.venue(), stream))
                .collect(),
        }
    }

    /// Every stream, in no particular order.
    pub fn all(&self) -> impl Iterator<Item = &Arc<LatestPrices>> {
        self.by_venue.values()
    }

    fn find(&self, venue: &str) -> Result<Arc<LatestPrices>, actix_web::Error> {
        let not_served = || ErrorNotFound(format!("no stream for venue '{venue}'"));
        let venue = PriceLevelVenue::from_str(venue).map_err(|_| not_served())?;
        self.by_venue
            .get(&venue)
            .cloned()
            .ok_or_else(not_served)
    }
}

/// Binds the HTTP API to `address` and returns the server to run, with the address it bound.
///
/// `derived` is `None` when the service does not run the market.
///
/// # Errors
///
/// Fails when `address` cannot be bound.
pub fn bind(
    streams: Arc<Streams>,
    derived: Option<Arc<DerivedSnapshots>>,
    address: SocketAddr,
) -> std::io::Result<(Server, SocketAddr)> {
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::from(streams.clone()))
            .app_data(web::Data::new(derived.clone()))
            .route("/v1/health", web::get().to(health))
            .route("/v1/price-levels/{venue}", web::get().to(current))
            .route("/v1/derived/{kind}", web::get().to(current_derived))
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
///
/// A service with no price-level streams is healthy once every derived computation has a
/// snapshot. The body lists the derived computations when the service runs the market.
async fn health(streams: web::Data<Streams>, derived: web::Data<DerivedStore>) -> impl Responder {
    let mut serving = Vec::new();
    let mut all = Vec::new();
    for stream in streams.all() {
        let name = stream.venue().to_string();
        if stream.has_prices() {
            serving.push(name.clone());
        }
        all.push(name);
    }
    serving.sort();
    all.sort();
    let mut body = json!({ "serving": serving, "streams": all });
    let mut healthy = !serving.is_empty();
    if let Some(derived) = derived.as_ref() {
        let held: Vec<&str> = DerivedKind::ALL
            .into_iter()
            .filter(|kind| derived.block(*kind).is_some())
            .map(DerivedKind::name)
            .collect();
        let computations = DerivedKind::ALL.map(DerivedKind::name);
        body["derived"] = json!({ "serving": held, "computations": computations });
        healthy = healthy || (all.is_empty() && held.len() == computations.len());
    }
    if healthy {
        HttpResponse::Ok().json(body)
    } else {
        HttpResponse::ServiceUnavailable().json(body)
    }
}

/// `GET /v1/price-levels/{venue}`: the stream's current message with its `ETag`.
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

/// `GET /v1/derived/{kind}`: the latest snapshot of the computation, with its block as `ETag`.
///
/// Answers 404 for an unknown kind or without the market, 204 until a snapshot is written and
/// 304 with no body when `If-None-Match` equals the `ETag`. The body is gzip-compressed when the
/// client accepts it.
async fn current_derived(
    request: HttpRequest,
    derived: web::Data<DerivedStore>,
    kind: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let not_served = || ErrorNotFound(format!("no derived data for '{kind}'"));
    let kind = DerivedKind::from_str(&kind).map_err(|_| not_served())?;
    let derived = derived
        .as_ref()
        .as_ref()
        .ok_or_else(not_served)?;
    let Some(snapshot) = derived.current(kind) else {
        return Ok(HttpResponse::NoContent().finish());
    };
    let etag = snapshot.etag();
    let not_modified = request
        .headers()
        .get(IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes());
    let mut response = if not_modified { HttpResponse::NotModified() } else { HttpResponse::Ok() };
    response
        .insert_header((ETAG, etag))
        .insert_header((VARY, "Accept-Encoding"));
    if not_modified {
        return Ok(response.finish());
    }
    response.insert_header(ContentType::json());
    if accepts_gzip(&request) {
        return Ok(response
            .insert_header(ContentEncoding::Gzip)
            .body(snapshot.gzip));
    }
    Ok(response.body(snapshot.json))
}

/// Whether the request's `Accept-Encoding` lists gzip with a quality above zero.
fn accepts_gzip(request: &HttpRequest) -> bool {
    let Some(AcceptEncoding(codings)) = request.get_header::<AcceptEncoding>() else {
        return false;
    };
    codings.iter().any(|coding| {
        coding.item == Preference::Specific(Encoding::gzip()) && coding.quality > Quality::ZERO
    })
}
