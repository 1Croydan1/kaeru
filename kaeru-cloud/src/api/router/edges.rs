//! Edge endpoints — the cloud's graph structure surface.
//!
//! - `POST /api/v1/edges` — ingest an edge between two shared nodes. The
//!   local daemon calls this after both endpoints are shared, so the graph
//!   structure survives `share` / `pull`, not just the nodes.
//! - `DELETE /api/v1/edges` — **retract** one edge, by `src` / `dst` /
//!   `edge_type`. Until this existed a cloud edge could only die by
//!   retracting one of its endpoint *nodes*, so a local `unlink` had nowhere
//!   to send itself and the next `pull` put the edge straight back — a
//!   retraction any pull can undo is not a retraction (#85).
//!
//! Per-initiative listing lives at
//! `GET /api/v1/initiatives/{name}/edges` (see `initiatives.rs`), the
//! counterpart a puller reads to rebuild edges locally.
//!
//! Gates with the `Authenticated` extractor and delegates straight to
//! `kaeru-core` — no business logic in between.

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use kaeru_core::{EdgeType, Store, unlink, upsert_edge};
use serde::{Deserialize, Serialize};

use crate::api::extractors::Authenticated;
use crate::api::state::AppState;
use crate::errors::ApiError;

pub fn edges_router() -> Router<AppState> {
    Router::new().route("/", post(ingest_edge).delete(retract_edge))
}

/// An edge pushed up from a local vault. `src` / `dst` are the (preserved)
/// node UUIDv7s — both must already be shared so the edge resolves.
#[derive(Debug, Deserialize)]
pub struct EdgeIngestReq {
    pub src: String,
    pub dst: String,
    pub edge_type: String,
    /// Connection strength in `[0, 1]`. Carried so the cloud preserves it
    /// across share / pull; re-posting an edge with a new weight is also the
    /// cloud-side edit handle. Defaults to `1.0` when omitted.
    #[serde(default = "default_weight")]
    pub weight: f64,
}

fn default_weight() -> f64 {
    1.0
}

#[derive(Debug, Serialize)]
pub struct EdgeView {
    pub src: String,
    pub dst: String,
    pub edge_type: String,
    pub weight: f64,
}

async fn ingest_edge(
    _: Authenticated,
    State(store): State<Arc<Store>>,
    Json(req): Json<EdgeIngestReq>,
) -> Result<(StatusCode, Json<EdgeView>), ApiError> {
    let edge_type =
        EdgeType::from_str(&req.edge_type).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if req.src.trim().is_empty() || req.dst.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "src and dst must not be empty".to_string(),
        ));
    }

    upsert_edge(&store, &req.src, &req.dst, edge_type, req.weight)?;

    Ok((
        StatusCode::CREATED,
        Json(EdgeView {
            src: req.src,
            dst: req.dst,
            edge_type: edge_type.as_str().to_string(),
            weight: req.weight.clamp(0.0, 1.0),
        }),
    ))
}

/// Which edge to retract. The triple is the edge's identity — the `edge`
/// relation is keyed by `{src, dst, edge_type, validity}`, so nothing else is
/// needed to name one.
#[derive(Debug, Deserialize)]
pub struct EdgeRetractReq {
    pub src: String,
    pub dst: String,
    pub edge_type: String,
}

/// `DELETE /api/v1/edges` — retract one edge, bi-temporally.
///
/// The body carries the triple rather than the path, because a path segment
/// cannot hold two UUIDs and a type without inventing an encoding for them.
///
/// Retraction, not erasure, exactly like `DELETE /nodes/{id}`: the edge stops
/// resolving at NOW and leaves the initiative's edge listing, while a read at
/// a past moment still traverses it.
///
/// Idempotent — retracting an edge that is already gone answers 204, so a
/// caller retrying after a dropped connection is not told its own success was
/// a failure.
///
/// **Whole-second caveat**, as everywhere: an edge retracted inside the same
/// second it was ingested carries an assert and a retract that cannot be
/// ordered, and may still read until the next write. Retrying a second later
/// settles it.
async fn retract_edge(
    _: Authenticated,
    State(store): State<Arc<Store>>,
    Json(req): Json<EdgeRetractReq>,
) -> Result<StatusCode, ApiError> {
    let edge_type =
        EdgeType::from_str(&req.edge_type).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if req.src.trim().is_empty() || req.dst.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "src and dst must not be empty".to_string(),
        ));
    }

    unlink(&store, &req.src, &req.dst, edge_type)?;
    Ok(StatusCode::NO_CONTENT)
}
