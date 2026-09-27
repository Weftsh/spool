//! Webhook receiver: verified push events fan out to background syncs.
//! Delivery → serving freshness is the M1 p50 < 10 s contract; the 60 s
//! poller (poller.rs) is the loss-recovery floor.
//!
//! GitHub sends every event the App subscribes to through this one
//! URL, told apart by `X-GitHub-Event`. A push takes the path below;
//! anything else — `installation`, `installation_repositories`, a job
//! event from an App that still subscribes to one — is acknowledged
//! after the same signature check and otherwise ignored. The branch is
//! load-bearing: without it any delivery that carries
//! `repository.full_name` would fall into the push path and re-sync
//! the mirror on an event that changed no ref.

use crate::app::SharedState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use stratum_control::registry;

pub async fn receive(
    State(state): State<SharedState>,
    Path(provider_name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(provider) = state.sync.provider_by_name(&provider_name) else {
        return (StatusCode::NOT_FOUND, "unknown webhook provider\n").into_response();
    };
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    // A missing header is a push: the generic provider sends none, and
    // that is the shape every delivery had before the App subscribed to
    // anything else.
    let event_name = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("push");
    if provider_name == "github" && event_name != "push" {
        if let Err(e) = provider.verify_signature(signature, &body) {
            return (StatusCode::UNAUTHORIZED, format!("{e}\n")).into_response();
        }
        // Acknowledged so GitHub does not count it as a failed delivery;
        // nothing is cached from these, so there is nothing to update.
        return (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({ "ignored": event_name })),
        )
            .into_response();
    }
    let event = match provider.verify_webhook(signature, &body) {
        Ok(e) => e,
        Err(e) => return (StatusCode::UNAUTHORIZED, format!("{e}\n")).into_response(),
    };
    let mirrors = match registry::mirrors_by_origin(&state.db, &provider_name, &event.full_name) {
        Ok(m) => m,
        Err(e) => return crate::api::internal(e),
    };
    let matched = mirrors.len();
    let received = std::time::Instant::now();
    for repo in mirrors {
        let state = state.clone();
        tokio::spawn(async move {
            match state.sync.sync(&repo).await {
                Ok(_) => {
                    // M6 freshness lag: webhook receipt -> serving state.
                    state.meter.record(
                        &repo.id,
                        "freshness",
                        0,
                        Some(received.elapsed().as_millis() as u64),
                    );
                }
                Err(e) => eprintln!("weft: webhook sync of {} failed: {e}", repo.id),
            }
        });
    }
    (
        StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({ "matched": matched })),
    )
        .into_response()
}
