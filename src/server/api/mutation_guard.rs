//! Shared gate for secret- or exec-bearing config mutations.

use axum::extract::FromRequestParts;
use axum::http::{request::Parts, StatusCode};
use axum::response::Response;

use super::{api_error, AppState};
use crate::server::auth::{handler_elevated, AuthenticatedSession, LocalAuthorization};

async fn mutation_gate(
    state: &AppState,
    session: Option<&AuthenticatedSession>,
    local_owner: bool,
) -> Result<(), Response> {
    if state.read_only {
        return Err(super::read_only_response());
    }
    if let Some(response) = super::cityhall_block(state) {
        return Err(response);
    }
    if !handler_elevated(state, session, local_owner).await {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "elevation_required",
            "Re-enter the passphrase to continue",
        ));
    }
    Ok(())
}

/// Parts-only extractor that rejects forbidden mutations before Axum reads or
/// parses the request body.
pub struct ElevatedMutationGuard;

impl FromRequestParts<std::sync::Arc<AppState>> for ElevatedMutationGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let session = parts.extensions.get::<AuthenticatedSession>();
        let local_owner = parts.extensions.get::<LocalAuthorization>().is_some();
        mutation_gate(state, session, local_owner).await?;
        Ok(Self)
    }
}
