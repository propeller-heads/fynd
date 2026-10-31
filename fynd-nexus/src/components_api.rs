//! The component routes of the HTTP API, served when the health monitor is on.
//!
//! - `GET /v1/components/excluded`: the excluded components stream, as the RFQ routes serve theirs
//! - `GET /v1/components?status=&protocol=`: the health of every component, as plain JSON
//! - `GET /v1/components/{id}`: the health of one component
//! - `DELETE /v1/components/health`: resets every component to unknown
//! - `DELETE /v1/components/{id}/health`: resets one component to unknown

use actix_web::{
    error::{ErrorNotFound, ErrorServiceUnavailable},
    web, HttpRequest, HttpResponse, Responder,
};
use chrono::{DateTime, Utc};
use fynd_core::ComponentId;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    api::{stream_response, Streams},
    monitor::{
        component_health::{ComponentHealth, HealthStatus},
        Monitor,
    },
    stream_kind::StreamKind,
};

/// Adds the component routes to the app.
pub fn configure(config: &mut web::ServiceConfig) {
    config
        .route("/v1/components/excluded", web::get().to(excluded))
        .route("/v1/components/health", web::delete().to(reset_all))
        .route("/v1/components", web::get().to(report))
        .route("/v1/components/{id}", web::get().to(one))
        .route("/v1/components/{id}/health", web::delete().to(reset_one));
}

/// The health of one component, with when it is probed next.
#[derive(Debug, Serialize, Deserialize)]
pub struct ComponentReport {
    /// The component id.
    pub id: ComponentId,
    /// The health record of the component.
    #[serde(flatten)]
    pub health: ComponentHealth,
    /// When the component is due for its next probe, or `None` if it cannot be probed.
    pub next_check_at: Option<DateTime<Utc>>,
}

impl ComponentReport {
    fn of(monitor: &Monitor, id: ComponentId, health: ComponentHealth) -> Self {
        let next_check_at = monitor.scheduler.next_check_at(&health);
        Self { id, health, next_check_at }
    }
}

/// The filters of `GET /v1/components`.
#[derive(Debug, Deserialize)]
struct ReportQuery {
    status: Option<HealthStatus>,
    protocol: Option<String>,
}

/// What the monitor adds to `GET /v1/health`: the number of components per status, and how long
/// ago the last probe gave a verdict.
pub fn monitor_health(monitor: &Monitor) -> Value {
    let mut by_status: Value = json!({});
    let mut total = 0;
    for status in HealthStatus::ALL {
        by_status[status.name()] = json!(0);
    }
    for ((_, status), count) in monitor.store.status_counts() {
        let previous = by_status[status.name()]
            .as_u64()
            .unwrap_or(0);
        by_status[status.name()] = json!(previous + count as u64);
        total += count;
    }
    json!({
        "components": total,
        "by_status": by_status,
        "last_probe_age_seconds": monitor.liveness.since_last_probe().as_secs(),
        "stalled": monitor.liveness.is_stalled(),
    })
}

/// `GET /v1/components/excluded`: the excluded components stream's current message.
async fn excluded(
    request: HttpRequest,
    streams: web::Data<Streams>,
) -> actix_web::Result<HttpResponse> {
    let stream = streams
        .get(StreamKind::ExcludedComponents)
        .ok_or_else(|| ErrorServiceUnavailable("the excluded components stream is not running"))?;
    Ok(stream_response(&request, &stream))
}

/// `GET /v1/components`: the health of the components, filtered by `status` and `protocol`.
async fn report(monitor: web::Data<Monitor>, query: web::Query<ReportQuery>) -> impl Responder {
    let components: Vec<ComponentReport> = monitor
        .store
        .report(query.status, query.protocol.as_deref())
        .into_iter()
        .map(|(id, health)| ComponentReport::of(&monitor, id, health))
        .collect();
    HttpResponse::Ok().json(json!({ "components": components }))
}

/// `GET /v1/components/{id}`: the health of one component, or 404.
async fn one(
    monitor: web::Data<Monitor>,
    id: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let id = id.to_lowercase();
    let health = monitor
        .store
        .get(&id)
        .ok_or_else(|| ErrorNotFound(format!("no tracked component '{id}'")))?;
    Ok(HttpResponse::Ok().json(ComponentReport::of(&monitor, id, health)))
}

/// `DELETE /v1/components/health`: resets every component to unknown.
async fn reset_all(monitor: web::Data<Monitor>) -> impl Responder {
    HttpResponse::Ok().json(json!({ "reset": monitor.reset_all() }))
}

/// `DELETE /v1/components/{id}/health`: resets one component to unknown, or 404.
async fn reset_one(
    monitor: web::Data<Monitor>,
    id: web::Path<String>,
) -> actix_web::Result<HttpResponse> {
    let id = id.to_lowercase();
    if !monitor.reset_one(&id) {
        return Err(ErrorNotFound(format!("no tracked component '{id}'")));
    }
    Ok(HttpResponse::Ok().json(json!({ "reset": 1 })))
}
