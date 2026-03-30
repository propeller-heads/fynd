use std::{
    future::{ready, Future, Ready},
    pin::Pin,
    time::Instant,
};

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use metrics::{counter, gauge, histogram};

pub struct MetricsMiddleware;

impl<S, B> Transform<S, ServiceRequest> for MetricsMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = MetricsMiddlewareService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(MetricsMiddlewareService { service }))
    }
}

pub struct MetricsMiddlewareService<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for MetricsMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &self,
        ctx: &mut core::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let method = req.method().to_string();
        let endpoint = req
            .match_pattern()
            .unwrap_or_else(|| req.path().to_string());

        gauge!("http_requests_in_flight", "method" => method.clone(), "endpoint" => endpoint.clone())
            .increment(1.0);

        let start = Instant::now();
        let fut = self.service.call(req);

        Box::pin(async move {
            let result = fut.await;
            let duration = start.elapsed().as_secs_f64();

            let status = match &result {
                Ok(resp) => resp.status().as_u16().to_string(),
                Err(err) => err
                    .as_response_error()
                    .status_code()
                    .as_u16()
                    .to_string(),
            };

            histogram!("http_request_duration_seconds", "method" => method.clone(), "endpoint" => endpoint.clone(), "status" => status.clone())
                .record(duration);
            counter!("http_requests_total", "method" => method.clone(), "endpoint" => endpoint.clone(), "status" => status)
                .increment(1);
            gauge!("http_requests_in_flight", "method" => method, "endpoint" => endpoint)
                .decrement(1.0);

            result
        })
    }
}
