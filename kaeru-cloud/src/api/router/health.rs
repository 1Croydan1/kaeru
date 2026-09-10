//! Liveness probe. Unauthenticated, no substrate access — just confirms the
//! service is up and reports the build version.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::state::AppState;

pub const HEALTH_TAG: &str = "health";

pub fn health_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(health))
}

/// What `/health` answers. A local daemon reads `core_version` to warn when it
/// and the cloud run different kaeru-core versions.
#[derive(Debug, Serialize, ToSchema)]
pub struct HealthView {
    /// Always `"ok"` when the service answers at all.
    pub status: String,
    /// Always `"kaeru-cloud"`.
    pub service: String,
    /// The kaeru-core version this cloud was built from.
    pub core_version: String,
}

#[utoipa::path(
    get,
    path = "/",
    tag = HEALTH_TAG,
    responses((status = 200, description = "The service is up.", body = HealthView))
)]
async fn health() -> Json<HealthView> {
    Json(HealthView {
        status: "ok".to_string(),
        service: "kaeru-cloud".to_string(),
        core_version: kaeru_core::version().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use kaeru_core::Store;
    use tower::util::ServiceExt;

    use crate::api::router::api_router;
    use crate::api::state::AppState;

    /// `/health` reports the running kaeru-core version — the contract a daemon
    /// reads to warn on a mcp <-> cloud version skew (issue #30).
    #[tokio::test]
    async fn health_reports_core_version() {
        let app = api_router(AppState {
            api_token: Arc::from(""),
            store: Arc::new(Store::open_in_memory().expect("open")),
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["core_version"].as_str(), Some(kaeru_core::version()));
    }
}
