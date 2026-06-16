//! The admin server-overview endpoint: cross-cutting server stats that do not
//! belong to one cube. Today it returns the view-cache counters (ADR-0028) for
//! the Server Overview dashboard; it is the home for future server-wide metrics.

use axum::extract::State;
use axum::Json;

use crate::auth::AuthPrincipal;
use crate::authz::require_admin;
use crate::dto::{OverviewDto, ViewCacheStatsDto};
use crate::{ApiError, AppState};

/// `GET /api/v1/overview` -> server-wide stats (admin only).
pub(crate) async fn overview(
    auth: AuthPrincipal,
    State(state): State<AppState>,
) -> Result<Json<OverviewDto>, ApiError> {
    require_admin(&state, &auth)?;
    Ok(Json(OverviewDto {
        view_cache: ViewCacheStatsDto {
            enabled: state.view_cache.enabled(),
            hits: state.view_cache.hits(),
            misses: state.view_cache.misses(),
            entries: state.view_cache.entries(),
        },
    }))
}
