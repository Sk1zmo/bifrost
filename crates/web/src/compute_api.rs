//! HTTP surface for shared compute.
//!
//! Everything the dashboard needs to show what the network can do, to pair
//! devices, and to submit work — kept apart from the chat and file routes
//! because it is a distinct feature with its own trust rules.

use std::collections::HashMap;

use axum::extract::{Path as RoutePath, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use bifrost_core::compute::{Priority, Work};
use serde::Deserialize;

use crate::{ApiError, ApiResult, AppState};

/// Compute routes, merged into the main router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/compute", get(overview))
        .route("/api/compute/toggle", post(toggle_sharing))
        .route("/api/compute/orchestrator", post(toggle_orchestrator))
        .route("/api/compute/pair/begin", post(pair_begin))
        .route("/api/compute/pair/with", post(pair_with))
        .route("/api/compute/revoke", post(revoke))
        .route("/api/compute/run", post(run))
        .route("/api/compute/job/:job_id", get(job).delete(cancel))
}

/// What the whole network can do, plus this node's own settings.
async fn overview(State(state): State<AppState>) -> impl IntoResponse {
    let node = &state.host;
    Json(serde_json::json!({
        "enabled": node.compute.is_enabled().await,
        "orchestrator_enabled": node.orchestrator.is_enabled().await,
        "requires_pairing": node.compute.trust.requires_pairing().await,
        // Only present while a code is on screen and unexpired.
        "pairing_code": node.compute.trust.current_code().await,
        "trusted": node.compute.trust.list().await,
        "capacity": node.orchestrator.capacity().await,
        "devices": node.orchestrator.devices().await,
        "jobs": node.orchestrator.jobs().await,
        "running_here": node.compute.running_jobs().await,
    }))
}

#[derive(Deserialize)]
struct Toggle {
    enabled: bool,
}

/// Turn this node's resource sharing on or off.
async fn toggle_sharing(
    State(state): State<AppState>,
    Json(body): Json<Toggle>,
) -> ApiResult<impl IntoResponse> {
    state.host.compute.set_enabled(body.enabled).await;
    state
        .host
        .persist_compute_settings(Some(body.enabled), None)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;

    Ok(Json(serde_json::json!({
        "text": if body.enabled {
            "This device is now sharing its resources"
        } else {
            "This device has stopped sharing its resources"
        }
    })))
}

/// Turn network-wide placement on or off.
async fn toggle_orchestrator(
    State(state): State<AppState>,
    Json(body): Json<Toggle>,
) -> ApiResult<impl IntoResponse> {
    state.host.orchestrator.set_enabled(body.enabled).await;
    state
        .host
        .persist_compute_settings(None, Some(body.enabled))
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;

    Ok(Json(serde_json::json!({
        "text": if body.enabled {
            "Work will be placed across the network"
        } else {
            "Work will no longer be placed; anything running continues"
        }
    })))
}

/// Show a code for the owner to read out to another device.
async fn pair_begin(State(state): State<AppState>) -> impl IntoResponse {
    let code = state.host.compute.trust.begin_pairing().await;
    Json(serde_json::json!({
        "code": code,
        "expires_in_secs": bifrost_core::compute::protocol::PAIRING_CODE_TTL_SECS,
        "text": "Read this code to the device that wants to use this one",
    }))
}

#[derive(Deserialize)]
struct PairWith {
    /// The device whose resources we want to use.
    peer: String,
    /// The code shown on that device's screen.
    code: String,
}

async fn pair_with(
    State(state): State<AppState>,
    Json(body): Json<PairWith>,
) -> ApiResult<impl IntoResponse> {
    let node = &state.host;

    let peer = node
        .registry
        .resolve(&body.peer)
        .await
        .ok_or_else(|| ApiError::not_found("no device matching that name"))?;

    let addr = peer
        .compute_addr()
        .ok_or_else(|| ApiError::bad_request("that device is not offering compute"))?;

    let token = node
        .compute
        .pair_with(addr, &body.code)
        .await
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    node.remember_pairing(&peer.node_id, &token)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;

    Ok(Json(serde_json::json!({
        "text": format!("Paired with {}", peer.display_name()),
    })))
}

#[derive(Deserialize)]
struct Revoke {
    peer: String,
}

async fn revoke(
    State(state): State<AppState>,
    Json(body): Json<Revoke>,
) -> ApiResult<impl IntoResponse> {
    if state.host.compute.trust.revoke(&body.peer).await {
        Ok(Json(
            serde_json::json!({ "text": "That device may no longer use this one" }),
        ))
    } else {
        Err(ApiError::not_found("that device was not paired"))
    }
}

#[derive(Deserialize)]
struct Run {
    /// A model name, for inference.
    #[serde(default)]
    model: Option<String>,
    /// A declared task name, for a job.
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    parameters: HashMap<String, String>,
    #[serde(default)]
    needs_gpu: bool,
    /// `background`, `normal` or `interactive`.
    #[serde(default)]
    priority: Option<String>,
}

/// Submit work. The orchestrator decides which device runs it.
async fn run(State(state): State<AppState>, Json(body): Json<Run>) -> ApiResult<impl IntoResponse> {
    let work = match (&body.model, &body.task) {
        (Some(model), _) => Work::Inference {
            model: model.clone(),
            prompt: body.prompt.clone().unwrap_or_default(),
            system: body.system.clone(),
            temperature: body.temperature,
        },
        (_, Some(task)) => Work::Task {
            name: task.clone(),
            parameters: body.parameters.clone(),
            needs_gpu: body.needs_gpu,
        },
        _ => return Err(ApiError::bad_request("give either a model or a task")),
    };

    let priority = match body.priority.as_deref() {
        Some("background") => Priority::Background,
        Some("interactive") => Priority::Interactive,
        _ => Priority::Normal,
    };

    // Work out where it would go before committing, so a request that nothing
    // can run fails immediately with a reason rather than sitting in a queue
    // that will never drain.
    let plan = state.host.orchestrator.plan(&work, &[]).await;
    let Some(best) = plan.first() else {
        return Err(ApiError::bad_request(
            "no device on this network can run that. Check the name, and that the \
             device offering it is sharing its resources",
        ));
    };
    let (worker, reason) = (best.display_name.clone(), best.reason.clone());

    let job_id = state
        .host
        .submit_work(work, priority)
        .await
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    Ok(Json(serde_json::json!({
        "job_id": job_id,
        "planned_for": worker,
        "reason": reason,
        "text": format!("Queued for {worker} — {reason}"),
    })))
}

/// One job, with whatever output has arrived so far.
async fn job(
    State(state): State<AppState>,
    RoutePath(job_id): RoutePath<String>,
) -> ApiResult<impl IntoResponse> {
    match state.host.orchestrator.job(&job_id).await {
        Some(job) => Ok(Json(job)),
        None => Err(ApiError::not_found("no such job")),
    }
}

async fn cancel(
    State(state): State<AppState>,
    RoutePath(job_id): RoutePath<String>,
) -> ApiResult<impl IntoResponse> {
    if state.host.orchestrator.cancel(&job_id).await {
        Ok(Json(serde_json::json!({ "text": "Cancelled" })))
    } else {
        Err(ApiError::not_found("that job is not running"))
    }
}
