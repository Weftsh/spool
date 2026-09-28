//! Organizations: creating one.
//!
//! An organization belongs to a person — a service token has no one to
//! own the result — so this is the one org route that insists on a
//! browser session rather than any credential.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct CreateOrgBody {
    pub name: String,
}

/// The signed-in person, or a refusal.
fn person(
    state: &SharedState,
    headers: &HeaderMap,
) -> Result<stratum_control::users::User, Response> {
    let Some(cookie) = authx::session_from_headers(headers) else {
        return Err(json_error(StatusCode::UNAUTHORIZED, "not signed in"));
    };
    let session = match stratum_control::sessions::verify(&state.db, cookie) {
        Ok(Some(s)) => s,
        Ok(None) => return Err(json_error(StatusCode::UNAUTHORIZED, "not signed in")),
        Err(e) => return Err(internal(e)),
    };
    match stratum_control::users::by_id(&state.db, &session.user_id) {
        Ok(Some(u)) => Ok(u),
        Ok(None) => Err(internal("session user vanished".into())),
        Err(e) => Err(internal(e)),
    }
}

/// Create an organization, owned by the person asking.
pub async fn create_org(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<CreateOrgBody>,
) -> Response {
    let user = match person(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let name = body.name.trim().to_string();
    if let Err(e) = stratum_control::registry::valid_namespace_name("organization", &name) {
        return json_error(StatusCode::BAD_REQUEST, e);
    }
    let org = match stratum_control::registry::create_org_owned_by(&state.db, &name, &user.id) {
        Ok(o) => o,
        Err(e) if stratum_engine::errclass::is_already_exists(&e) => {
            return json_error(StatusCode::CONFLICT, e)
        }
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
    };
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": org.id, "name": org.name })),
    )
        .into_response()
}
