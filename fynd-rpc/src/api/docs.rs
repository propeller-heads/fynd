//! OpenAPI spec construction and Swagger UI registration.
//!
//! Two specs are served from the same binary:
//!
//! - `/docs/` + `/api-docs/openapi.json` — self-hosted deployments, describing the paths this
//!   process routes (`/v1/quote`) on the origin it is reached at, with no authentication.
//! - `/docs/hosted/` + `/api-docs/hosted/openapi.json` — the hosted gateway, which routes per chain
//!   (`/v1/{chain}/quote`) and authenticates requests with an API key sent as the raw
//!   `Authorization` header value. Only served when a gateway URL is configured.

use actix_web::web;
use serde_json::json;
use utoipa::{
    openapi::{
        path::{Operation, Parameter, ParameterBuilder, ParameterIn},
        security::{ApiKey, ApiKeyValue, SecurityScheme},
        Components, ObjectBuilder, OpenApi, PathItem, Required, SecurityRequirement, Server, Type,
    },
    OpenApi as _,
};
use utoipa_swagger_ui::SwaggerUi;

use crate::api::ApiDoc;
#[cfg(feature = "experimental")]
use crate::api::ExperimentalApiDoc;

/// Name of the API key security scheme declared by the hosted spec.
const API_KEY_SCHEME_NAME: &str = "ApiKeyAuth";

/// Registers the Swagger UIs and the specs they load.
///
/// The self-hosted UI is always served. The hosted one describes a gateway this process knows
/// nothing about, so it is only served when `hosted_url` names that gateway.
///
/// The hosted UI is registered first because actix matches resources in registration order:
/// `/docs/{_:.*}` also matches `/docs/hosted/index.html` and would serve a 404 for it.
pub(crate) fn configure_docs(cfg: &mut web::ServiceConfig, hosted_url: Option<&str>) {
    if let Some(hosted_url) = hosted_url {
        cfg.service(
            SwaggerUi::new("/docs/hosted/{_:.*}")
                .url("/api-docs/hosted/openapi.json", hosted_spec(hosted_url)),
        );
    }
    cfg.service(SwaggerUi::new("/docs/{_:.*}").url("/api-docs/openapi.json", self_hosted_spec()));
}

/// Builds the spec describing the endpoints this process routes.
fn self_hosted_spec() -> OpenApi {
    #[allow(unused_mut)]
    let mut openapi = ApiDoc::openapi();
    #[cfg(feature = "experimental")]
    openapi.merge(ExperimentalApiDoc::openapi());
    openapi
}

/// Builds the spec describing the endpoints as exposed by the hosted gateway.
///
/// The gateway picks a backend from a chain segment in the path, so every `/v1/<endpoint>` becomes
/// `/v1/{chain}/<endpoint>` and gains a `chain` path parameter. It also authenticates every
/// request; declaring the security scheme is what gives Swagger UI its "Authorize" button, without
/// which "Try it out" can only produce 401s. The gateway matches the entire `Authorization`
/// header value against its key store, so the scheme is an API key in that header — an HTTP
/// bearer scheme would add a `Bearer ` prefix the gateway rejects.
fn hosted_spec(server_url: &str) -> OpenApi {
    let mut openapi = self_hosted_spec();

    let routed_paths = std::mem::take(&mut openapi.paths.paths);
    for (path, mut path_item) in routed_paths {
        let Some(endpoint) = path.strip_prefix("/v1/") else {
            openapi
                .paths
                .paths
                .insert(path, path_item);
            continue
        };
        for operation in operations_mut(&mut path_item) {
            operation
                .parameters
                .get_or_insert_with(Vec::new)
                .push(chain_parameter())
        }
        openapi
            .paths
            .paths
            .insert(format!("/v1/{{chain}}/{endpoint}"), path_item);
    }

    openapi
        .components
        .get_or_insert_with(Components::new)
        .add_security_scheme(
            API_KEY_SCHEME_NAME,
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "Authorization",
                "Fynd API key, sent as the raw header value (no `Bearer ` prefix).",
            ))),
        );
    openapi.security =
        Some(vec![SecurityRequirement::new(API_KEY_SCHEME_NAME, Vec::<String>::new())]);
    openapi.servers = Some(vec![Server::new(server_url)]);

    openapi
}

/// Path parameter naming the chain the hosted gateway routes to.
fn chain_parameter() -> Parameter {
    ParameterBuilder::new()
        .name("chain")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("Chain to route the request to."))
        .schema(Some(ObjectBuilder::new().schema_type(Type::String)))
        .example(Some(json!("ethereum")))
        .build()
}

/// Every operation defined on a path item, whichever HTTP methods it declares.
fn operations_mut(path_item: &mut PathItem) -> impl Iterator<Item = &mut Operation> {
    [
        path_item.get.as_mut(),
        path_item.put.as_mut(),
        path_item.post.as_mut(),
        path_item.delete.as_mut(),
        path_item.options.as_mut(),
        path_item.head.as_mut(),
        path_item.patch.as_mut(),
        path_item.trace.as_mut(),
    ]
    .into_iter()
    .flatten()
}

#[cfg(test)]
mod tests {
    // Aliased: an unqualified `test` import shadows the built-in `#[test]` attribute with
    // actix-web's, which rejects non-async test functions.
    use actix_web::{http::StatusCode, test as actix_test, App};
    use serde_json::Value;

    use super::*;

    const TEST_SERVER_URL: &str = "https://fynd.test";

    /// The hosted spec as Swagger UI receives it.
    fn hosted_spec_json(server_url: &str) -> Value {
        serde_json::to_value(hosted_spec(server_url)).expect("spec serializes")
    }

    #[test]
    fn self_hosted_spec_paths() {
        let spec = self_hosted_spec();
        let paths: Vec<&str> = spec
            .paths
            .paths
            .keys()
            .map(String::as_str)
            .collect();

        assert!(paths.contains(&"/v1/quote"), "{paths:?}");
        assert!(paths.contains(&"/v1/health"), "{paths:?}");
        assert!(paths.contains(&"/v1/info"), "{paths:?}");
        assert!(
            paths
                .iter()
                .all(|path| !path.contains("{chain}")),
            "{paths:?}"
        )
    }

    #[test]
    fn self_hosted_spec_has_no_security() {
        let spec = self_hosted_spec();

        assert!(spec.security.is_none());
        assert!(spec.servers.is_none());
        assert!(spec
            .components
            .expect("schemas are declared")
            .security_schemes
            .is_empty())
    }

    #[test]
    fn hosted_spec_paths_carry_chain_segment() {
        let spec = hosted_spec(TEST_SERVER_URL);
        let paths: Vec<&str> = spec
            .paths
            .paths
            .keys()
            .map(String::as_str)
            .collect();

        assert!(paths.contains(&"/v1/{chain}/quote"), "{paths:?}");
        assert!(paths.contains(&"/v1/{chain}/health"), "{paths:?}");
        assert!(paths.contains(&"/v1/{chain}/info"), "{paths:?}");
        assert!(
            paths
                .iter()
                .all(|path| path.starts_with("/v1/{chain}/")),
            "{paths:?}"
        )
    }

    #[cfg(feature = "experimental")]
    #[test]
    fn hosted_spec_includes_experimental_paths() {
        let spec = hosted_spec(TEST_SERVER_URL);

        assert!(spec
            .paths
            .paths
            .contains_key("/v1/{chain}/prices"))
    }

    #[test]
    fn hosted_spec_operations_declare_chain_parameter() {
        let spec = hosted_spec_json(TEST_SERVER_URL);

        let paths = spec["paths"]
            .as_object()
            .expect("paths are an object");
        for (path, path_item) in paths {
            for (method, operation) in path_item
                .as_object()
                .expect("path items are an object")
            {
                let parameters = operation["parameters"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{method} {path} declares no parameters"));
                let chain = parameters
                    .iter()
                    .find(|parameter| parameter["name"] == "chain")
                    .unwrap_or_else(|| panic!("{method} {path} declares no chain parameter"));
                assert_eq!(chain["in"], "path", "{method} {path}");
                assert_eq!(chain["required"], true, "{method} {path}")
            }
        }
    }

    /// The gateway matches the raw `Authorization` header value, so the scheme must make
    /// Swagger UI send the key without a `Bearer ` prefix.
    #[test]
    fn hosted_spec_declares_api_key_auth() {
        let spec = hosted_spec_json(TEST_SERVER_URL);

        let scheme = &spec["components"]["securitySchemes"]["ApiKeyAuth"];
        assert_eq!(scheme["type"], "apiKey", "{scheme}");
        assert_eq!(scheme["in"], "header", "{scheme}");
        assert_eq!(scheme["name"], "Authorization", "{scheme}");
        assert_eq!(spec["security"], json!([{"ApiKeyAuth": []}]))
    }

    #[test]
    fn hosted_spec_server_url() {
        let spec = hosted_spec(TEST_SERVER_URL);

        let servers = spec.servers.expect("server is set");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].url, TEST_SERVER_URL)
    }

    /// Both UIs must resolve: `/docs/{_:.*}` matches `/docs/hosted/...` too, so registration order
    /// decides whether the hosted UI is reachable at all.
    #[actix_web::test]
    async fn both_swagger_uis_are_reachable() {
        let app = actix_test::init_service(
            App::new().configure(|cfg| configure_docs(cfg, Some(TEST_SERVER_URL))),
        )
        .await;

        for path in ["/docs/", "/docs/hosted/"] {
            let request = actix_test::TestRequest::get()
                .uri(path)
                .to_request();
            let response = actix_test::call_service(&app, request).await;
            assert!(response.status().is_success(), "{path}: {}", response.status())
        }
    }

    /// Self-hosted deployments get no hosted UI, so nothing points them at a gateway they have
    /// no key for.
    #[actix_web::test]
    async fn hosted_swagger_ui_needs_a_url() {
        let app =
            actix_test::init_service(App::new().configure(|cfg| configure_docs(cfg, None))).await;

        let self_hosted = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri("/docs/")
                .to_request(),
        )
        .await;
        assert!(self_hosted.status().is_success(), "{}", self_hosted.status());

        for path in ["/docs/hosted/", "/api-docs/hosted/openapi.json"] {
            let request = actix_test::TestRequest::get()
                .uri(path)
                .to_request();
            let response = actix_test::call_service(&app, request).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}")
        }
    }

    #[actix_web::test]
    async fn spec_endpoints_serve_their_own_paths() {
        let app = actix_test::init_service(
            App::new().configure(|cfg| configure_docs(cfg, Some(TEST_SERVER_URL))),
        )
        .await;

        let self_hosted: Value = actix_test::call_and_read_body_json(
            &app,
            actix_test::TestRequest::get()
                .uri("/api-docs/openapi.json")
                .to_request(),
        )
        .await;
        assert!(self_hosted["paths"]["/v1/quote"].is_object(), "{self_hosted}");

        let hosted: Value = actix_test::call_and_read_body_json(
            &app,
            actix_test::TestRequest::get()
                .uri("/api-docs/hosted/openapi.json")
                .to_request(),
        )
        .await;
        assert!(hosted["paths"]["/v1/{chain}/quote"].is_object(), "{hosted}");
        assert!(
            hosted["components"]["securitySchemes"][API_KEY_SCHEME_NAME].is_object(),
            "{hosted}"
        )
    }
}
