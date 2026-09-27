//! Checking an origin before a mirror is created against it.
//!
//! Mirror creation answers 202 and walks away, so a typo surfaces
//! minutes later as `sync_error` on a repo that looks broken. This is the
//! check that happens while the person is still looking at the field.
//!
//! It is also the one endpoint that fetches a URL a stranger supplies,
//! so it is deliberately narrow: `org:admin` — creating mirrors is an
//! administrative act, and a read-only member has no reason to make the
//! server open connections — plus every guard in
//! [`crate::mirror::probe`], plus the rate limit below.

use crate::api::json_error;
use crate::app::SharedState;
use crate::authx;
use crate::mirror::probe;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use stratum_control::auth::Scope;

#[derive(Deserialize)]
pub struct ProbeBody {
    pub origin: String,
}

/// How long an origin gets to answer. Long enough for a slow forge on a
/// cold connection, short enough that a hanging origin does not hold a
/// request open past what any proxy in front of us would tolerate.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Probes allowed per org, per window.
///
/// The guard refuses private address space, so this is not what stops
/// the endpoint being an internal scanner — that is. This stops it being
/// an *external* one: a paid org walking the public internet at our
/// expense and from our address.
///
/// Thirty a minute is one every two seconds sustained, which is well
/// past what setting up a batch of mirrors takes and nowhere near what a
/// sweep needs. Set deliberately generous: a limit that a real admin
/// trips is a limit that gets raised in a hurry by whoever is on call.
const MAX_PER_WINDOW: usize = 30;
const WINDOW: Duration = Duration::from_secs(60);

/// Per-org probe timestamps.
///
/// Process-local, which is honest rather than ideal: a fleet of N tasks
/// allows N times this. The limit that has to hold across a fleet is the
/// guard, which is stateless; this one is about politeness and cost, and
/// a shared counter for it would buy a round trip to Postgres on every
/// keystroke-triggered probe.
static RECENT: Mutex<Option<HashMap<String, Vec<Instant>>>> = Mutex::new(None);

fn allow(org_id: &str) -> bool {
    let mut guard = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    // Drop everything that has aged out, for this org and for any org
    // that has stopped probing, so the map cannot grow without bound.
    map.retain(|_, times| {
        times.retain(|t| now.duration_since(*t) < WINDOW);
        !times.is_empty()
    });
    let times = map.entry(org_id.to_string()).or_default();
    if times.len() >= MAX_PER_WINDOW {
        return false;
    }
    times.push(now);
    true
}

pub async fn probe_origin(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ProbeBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        return r;
    }
    if !allow(&org.id) {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many origin checks — wait a moment and try again",
        );
    }
    // A probe is a question, not a change: it has no side effects and no
    // audit entry. What gets recorded is the mirror that comes of it.
    let found = probe::probe(&body.origin, TIMEOUT);
    // 200 whether or not the origin was reachable — the *probe*
    // succeeded either way, and the answer is in the body. A 4xx here
    // would make "this looks private", which is a normal and expected
    // outcome, indistinguishable from a malformed request.
    Json(found).into_response()
}
