use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig,
    session::{SessionManager, local::LocalSessionManager},
    tower::StreamableHttpService,
};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

use crate::{
    audit::AuditLogger,
    config::{AuthMode, BindAddress, Config},
    error::{AppError, Result},
    project::ProjectResolver,
    storage::Storage,
    tools::{AgentHandler, SharedState},
};

enum BoundListener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

async fn bind_listener(config: &Config) -> Result<BoundListener> {
    match &config.bind {
        BindAddress::Tcp(address) => tokio::net::TcpListener::bind(address)
            .await
            .map(BoundListener::Tcp)
            .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string())),
        BindAddress::Unix(path) => {
            #[cfg(unix)]
            {
                match std::fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.file_type().is_socket() => {
                        std::fs::remove_file(path).map_err(|error| {
                            AppError::new(
                                "PROCESS_FAILED",
                                format!(
                                    "cannot remove existing Unix socket {}: {error}",
                                    path.display()
                                ),
                            )
                        })?;
                    }
                    Ok(_) => {
                        return Err(AppError::new(
                            "PROCESS_FAILED",
                            format!(
                                "Unix socket path {} already exists and is not a socket",
                                path.display()
                            ),
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(AppError::new(
                            "PROCESS_FAILED",
                            format!("cannot inspect Unix socket {}: {error}", path.display()),
                        ));
                    }
                }

                let listener = tokio::net::UnixListener::bind(path).map_err(|error| {
                    AppError::new(
                        "PROCESS_FAILED",
                        format!("cannot bind Unix socket {}: {error}", path.display()),
                    )
                })?;
                if let Err(error) = std::fs::set_permissions(
                    path,
                    std::fs::Permissions::from_mode(config.unix_socket_mode),
                ) {
                    drop(listener);
                    let _ = std::fs::remove_file(path);
                    return Err(AppError::new(
                        "PROCESS_FAILED",
                        format!(
                            "cannot chmod Unix socket {} to {:04o}: {error}",
                            path.display(),
                            config.unix_socket_mode
                        ),
                    ));
                }
                Ok(BoundListener::Unix(listener))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                Err(AppError::config(
                    "Unix socket MCP_BIND values are not supported on this platform",
                ))
            }
        }
    }
}

#[derive(Clone)]
struct HttpState {
    auth_token: Arc<String>,
    auth_mode: AuthMode,
    audit: AuditLogger,
    active_requests: Arc<AtomicUsize>,
    max_active_requests: usize,
    session_manager: Arc<LocalSessionManager>,
    max_sessions: usize,
    session_activity: Arc<DashTimestamps>,
}

fn token_matches(candidate: &str, expected: &str) -> bool {
    candidate.len() == expected.len() && candidate.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn auth_matches(
    mode: AuthMode,
    path_candidate: &str,
    bearer_candidate: Option<&str>,
    expected: &str,
) -> bool {
    let path_authenticated = token_matches(path_candidate, expected);
    let bearer_authenticated =
        bearer_candidate.is_some_and(|candidate| token_matches(candidate, expected));
    match mode {
        AuthMode::Path => path_authenticated,
        AuthMode::Bearer => bearer_authenticated,
        AuthMode::Either => path_authenticated || bearer_authenticated,
    }
}

struct ActiveRequestGuard(Arc<AtomicUsize>);
impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn health(state: axum::extract::State<HttpState>) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "status": "ok",
        "active_requests": state.active_requests.load(Ordering::Relaxed),
        "active_legacy_mcp_sessions": state.session_manager.sessions.read().await.len(),
    }))
}

fn is_modern_stateless_protocol(version: &str) -> bool {
    let mut parts = version.split('-');
    let parsed = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(year), Some(month), Some(day), None) => {
            match (year.parse::<u16>(), month.parse::<u8>(), day.parse::<u8>()) {
                (Ok(year), Ok(month), Ok(day))
                    if (1..=12).contains(&month) && (1..=31).contains(&day) =>
                {
                    Some((year, month, day))
                }
                _ => None,
            }
        }
        _ => None,
    };
    parsed.is_some_and(|date| date >= (2026, 7, 28))
}

async fn authenticate_and_observe(
    state: axum::extract::State<HttpState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let state = state.0;
    let candidate = request
        .uri()
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    let bearer_candidate = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let authenticated = auth_matches(
        state.auth_mode,
        candidate,
        bearer_candidate,
        state.auth_token.as_str(),
    );
    if !authenticated {
        tracing::warn!(event = "auth_failed", "rejected MCP request");
        state.audit.emit(json!({"event":"mcp_request_finished","endpoint":"/[REDACTED]/mcp","status":"rejected","error":{"code":"AUTH_FAILED","message":"invalid endpoint authentication"}}));
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error":{"code":"AUTH_FAILED","message":"authentication failed"}})),
        )
            .into_response();
    }
    let modern_stateless = request
        .headers()
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_modern_stateless_protocol);
    let has_mcp_session = request.headers().contains_key("mcp-session-id");
    if let Some(mcp_session_id) = request
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
    {
        let exists = state
            .session_manager
            .sessions
            .read()
            .await
            .keys()
            .any(|id| id.as_ref() == mcp_session_id);
        if exists {
            state
                .session_activity
                .touch(mcp_session_id.to_owned(), Instant::now());
        }
    }
    if !modern_stateless
        && !has_mcp_session
        && state.session_manager.sessions.read().await.len() >= state.max_sessions
    {
        state
            .audit
            .emit(json!({"event":"server_busy","resource":"legacy_mcp_sessions","retryable":true}));
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":{"code":"SERVER_BUSY","message":"legacy MCP session capacity reached; retry later"}})),
        )
            .into_response();
    }
    let method = request.method().as_str().to_owned();
    let previous = state.active_requests.fetch_add(1, Ordering::Relaxed);
    if previous >= state.max_active_requests {
        state.active_requests.fetch_sub(1, Ordering::Relaxed);
        state.audit.emit(
            json!({"event":"server_busy","resource":"active_http_requests","retryable":true}),
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":{"code":"SERVER_BUSY","message":"request concurrency limit reached; retry later"}})),
        )
            .into_response();
    }
    let _active_guard = ActiveRequestGuard(state.active_requests.clone());
    let started = Instant::now();
    state.audit.emit(json!({"event":"mcp_request_started","endpoint":"/[REDACTED]/mcp","method":request.method().as_str(),"active_requests":state.active_requests.load(Ordering::Relaxed)}));
    let response = next.run(request).await;
    state.audit.emit(json!({"event":"mcp_request_finished","endpoint":"/[REDACTED]/mcp","method":method,"status_code":response.status().as_u16(),"duration_ms":started.elapsed().as_millis(),"active_requests":state.active_requests.load(Ordering::Relaxed).saturating_sub(1)}));
    response
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {_ = tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn session_maintenance(
    manager: Arc<LocalSessionManager>,
    maximum: usize,
    idle: Duration,
    cancellation: CancellationToken,
    audit: AuditLogger,
    seen: Arc<DashTimestamps>,
) {
    let interval = Duration::from_secs(1);
    loop {
        tokio::select! {_=cancellation.cancelled()=>break,_=tokio::time::sleep(interval)=>{}}
        let ids: Vec<_> = manager.sessions.read().await.keys().cloned().collect();
        let now = Instant::now();
        let live: std::collections::HashSet<String> = ids.iter().map(ToString::to_string).collect();
        seen.retain(&live);
        for id in &ids {
            seen.touch_if_missing(id.to_string(), now);
        }
        let mut expire: Vec<_> = ids
            .iter()
            .filter(|id| seen.age(id.as_ref(), now).is_some_and(|age| age >= idle))
            .cloned()
            .collect();
        if ids.len().saturating_sub(expire.len()) > maximum {
            let mut candidates: Vec<_> = ids
                .iter()
                .filter(|id| !expire.contains(id))
                .cloned()
                .collect();
            candidates.sort_by_key(|id| {
                std::cmp::Reverse(seen.age(id.as_ref(), now).unwrap_or_default())
            });
            let additional: Vec<_> = candidates
                .into_iter()
                .take(ids.len() - expire.len() - maximum)
                .collect();
            expire.extend(additional);
        }
        for id in expire {
            let _ = manager.close_session(&id).await;
            seen.remove(id.as_ref());
        }
        if manager.sessions.read().await.len() > maximum {
            audit.emit(json!({"event":"resource_limit_hit","resource":"legacy_mcp_sessions","maximum":maximum}));
        }
    }
}

#[derive(Default)]
struct DashTimestamps(dashmap::DashMap<String, Instant>);
impl DashTimestamps {
    fn touch(&self, key: String, now: Instant) {
        self.0.insert(key, now);
    }
    fn touch_if_missing(&self, key: String, now: Instant) {
        self.0.entry(key).or_insert(now);
    }
    fn age(&self, key: &str, now: Instant) -> Option<Duration> {
        self.0
            .get(key)
            .map(|value| now.saturating_duration_since(*value))
    }
    fn remove(&self, key: &str) {
        self.0.remove(key);
    }
    fn retain(&self, live: &std::collections::HashSet<String>) {
        self.0.retain(|key, _| live.contains(key));
    }
}

pub async fn run(config: Config) -> Result<()> {
    let config = Arc::new(config);
    std::fs::create_dir_all(&config.workspace_root)?;
    let storage = Storage::open(
        &config
            .workspace_root
            .join(".metadata")
            .join("agent.sqlite3"),
    )?;
    let resolver = ProjectResolver::new(config.workspace_root.clone(), storage.clone())?;
    let audit = AuditLogger::new(config.logs.clone(), config.auth_token.clone()).await?;
    // Upstreams are opt-in through MCP_UPSTREAM_CONFIG. Native tools remain a
    // fixed contract; direct/gateway routes are added only after explicit
    // operator configuration and keep the active project identity boundary.
    let upstreams = crate::upstream::connect_upstreams(&config).await;
    let native_tools = AgentHandler::native_tool_count();
    let upstream_tools = upstreams.aggregator.exposed_tool_count();
    let exposed_tools = native_tools + upstream_tools;
    tracing::info!(
        config = %config.diagnostic_summary(),
        native_tools,
        upstream_tools,
        exposed_tools,
        "MCP startup capability summary"
    );
    for report in upstreams.aggregator.report() {
        tracing::info!(upstream = %report, "upstream MCP status");
    }
    let shared = SharedState::new(
        config.clone(),
        resolver,
        storage,
        audit.clone(),
        upstreams.aggregator.clone(),
    );
    let cancellation = CancellationToken::new();
    let session_manager = Arc::new(LocalSessionManager::default());
    let session_activity = Arc::new(DashTimestamps::default());
    let factory_shared = shared.clone();
    let mut transport_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(true)
        .with_json_response(true)
        .with_max_request_body_bytes(config.limits.request_body_bytes)
        .with_cancellation_token(cancellation.child_token());
    transport_config = if config.allowed_hosts.is_empty() {
        transport_config.disable_allowed_hosts()
    } else {
        transport_config.with_allowed_hosts(config.allowed_hosts.clone())
    };
    let mcp_service: StreamableHttpService<AgentHandler, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(AgentHandler::new(factory_shared.clone())),
            session_manager.clone(),
            transport_config,
        );
    let active_requests = Arc::new(AtomicUsize::new(0));
    let http_state = HttpState {
        auth_token: Arc::new(config.auth_token.clone()),
        auth_mode: config.auth_mode,
        audit: audit.clone(),
        active_requests: active_requests.clone(),
        max_active_requests: config.limits.max_concurrent_tools,
        session_manager: session_manager.clone(),
        max_sessions: config.max_sessions,
        session_activity: session_activity.clone(),
    };
    let protected = match config.auth_mode {
        AuthMode::Path => Router::new().route_service("/{auth_token}/mcp", mcp_service),
        AuthMode::Bearer => Router::new().route_service("/mcp", mcp_service),
        AuthMode::Either => Router::new()
            .route_service("/{auth_token}/mcp", mcp_service.clone())
            .route_service("/mcp", mcp_service),
    }
    .layer(middleware::from_fn_with_state(
        http_state.clone(),
        authenticate_and_observe,
    ));
    let app = Router::new()
        .route("/health", get(health))
        .with_state(http_state)
        .merge(protected);
    let listener = bind_listener(&config).await?;
    let sandbox_backend = crate::sandbox::effective_default_sandbox_backend(&config);
    let auth_token_file = config.workspace_root.join(".metadata/auth-token");
    tracing::info!(
        bind = %config.bind,
        workspace = %config.workspace_root.display(),
        auth_mode = config.auth_mode.as_str(),
        auth_token_file = %auth_token_file.display(),
        sandbox_backend,
        native_tools,
        upstream_tools,
        exposed_tools,
        "CodexBridge ready"
    );
    audit.emit(json!({"event":"server_started","bind":config.bind.to_string(),"endpoint":"/[REDACTED]/mcp","workspace_root":config.workspace_root,"logs":config.logs.root,"stateless_mcp":true,"legacy_stateful_mcp":true,"yolo_tools":true,"native_tools":native_tools,"upstream_tools":upstream_tools,"exposed_tools":exposed_tools,"max_concurrent_tools":config.limits.max_concurrent_tools,"max_concurrent_processes":config.limits.max_concurrent_processes}));
    let maintenance = tokio::spawn(session_maintenance(
        session_manager.clone(),
        config.max_sessions,
        config.session_idle,
        cancellation.child_token(),
        audit.clone(),
        session_activity,
    ));
    let process_cleanup = {
        let interactive = shared.interactive.clone();
        let ct = cancellation.child_token();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = ct.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => interactive.cleanup(),
                }
            }
        })
    };
    let status_active_requests = active_requests.clone();
    let status_task = if config.status_interval.is_zero() {
        None
    } else {
        let audit = audit.clone();
        let shared = shared.clone();
        let sessions = session_manager.clone();
        let interval = config.status_interval;
        let ct = cancellation.child_token();
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {_=ct.cancelled()=>break,_=tokio::time::sleep(interval)=>audit.emit(json!({"event":"daemon_status","active_requests":status_active_requests.load(Ordering::Relaxed),"active_processes":shared.active_processes(),"active_legacy_mcp_sessions":sessions.sessions.read().await.len(),"running_tools":audit.running_count(),"tracked_process_tasks":shared.interactive.tracked_tasks(),"project_cache_entries":shared.project_cache_entries(),"dropped_log_events":audit.dropped_count(),"log_queue_capacity":audit.queue_capacity(),"log_queue_bytes_available":audit.queue_bytes_available()}))}
            }
        }))
    };
    let serve = match listener {
        BoundListener::Tcp(listener) => {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await
        }
        #[cfg(unix)]
        BoundListener::Unix(listener) => {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await
        }
    };
    audit.emit(json!({"event":"server_stopping","active_requests":active_requests.load(Ordering::Relaxed),"active_processes":shared.active_processes(),"active_legacy_mcp_sessions":session_manager.sessions.read().await.len()}));
    cancellation.cancel();
    shared.interactive.shutdown();
    tracing::info!("CodexBridge shutdown started");
    let _ = tokio::time::timeout(Duration::from_secs(10), maintenance).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), process_cleanup).await;
    if let Some(status_task) = status_task {
        let _ = status_task.await;
    }
    let ids: Vec<_> = session_manager
        .sessions
        .read()
        .await
        .keys()
        .cloned()
        .collect();
    for id in ids {
        let _ = session_manager.close_session(&id).await;
    }
    let (shutdown_requested_processes, remaining_processes) = shared
        .interactive
        .shutdown_and_wait(Duration::from_secs(2))
        .await;
    audit.emit(json!({
        "event":"server_stopped",
        "shutdown_requested_processes": shutdown_requested_processes,
        "remaining_tasks":audit.running_count(),
        "remaining_processes":remaining_processes,
        "remaining_sessions":session_manager.sessions.read().await.len()
    }));
    audit.shutdown().await;
    drop(upstreams.services);
    serve.map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use crate::config::ConfigBuilder;
    #[cfg(unix)]
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_replaces_stale_socket_and_applies_mode() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("codexbridge.sock");
        let config = ConfigBuilder::from_map(BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_BIND".to_owned(), socket_path.display().to_string()),
            ("MCP_UNIX_SOCKET_MODE".to_owned(), "0750".to_owned()),
        ]))
        .build()
        .unwrap();

        let first = bind_listener(&config).await.unwrap();
        assert!(matches!(first, BoundListener::Unix(_)));
        assert_eq!(
            std::fs::metadata(&socket_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        drop(first);

        let second = bind_listener(&config).await.unwrap();
        assert!(matches!(second, BoundListener::Unix(_)));
        assert_eq!(
            std::fs::metadata(&socket_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_preserves_existing_non_socket_path() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("codexbridge.sock");
        std::fs::write(&socket_path, b"do not delete").unwrap();
        let config = ConfigBuilder::from_map(BTreeMap::from([
            ("MCP_AUTH_TOKEN".to_owned(), "1234567890abcdef".to_owned()),
            ("MCP_BIND".to_owned(), socket_path.display().to_string()),
        ]))
        .build()
        .unwrap();

        let error = match bind_listener(&config).await {
            Ok(_) => panic!("regular file must not be replaced by a Unix socket"),
            Err(error) => error,
        };
        assert!(error.message().contains("is not a socket"));
        assert_eq!(std::fs::read(&socket_path).unwrap(), b"do not delete");
    }

    #[test]
    fn session_activity_touch_tracks_last_activity_not_creation_age() {
        let timestamps = DashTimestamps::default();
        let started = Instant::now();
        timestamps.touch("transport-a".into(), started);
        timestamps.touch("transport-a".into(), started + Duration::from_secs(10));
        assert_eq!(
            timestamps.age("transport-a", started + Duration::from_secs(15)),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn protocol_version_comparison_is_structured_not_lexical() {
        assert!(is_modern_stateless_protocol("2026-07-28"));
        assert!(is_modern_stateless_protocol("2027-01-01"));
        assert!(!is_modern_stateless_protocol("2026-07-27"));
        assert!(!is_modern_stateless_protocol("2026-13-01"));
        assert!(!is_modern_stateless_protocol("2026-00-01"));
        assert!(!is_modern_stateless_protocol("2026-01-00"));
        assert!(!is_modern_stateless_protocol("2026-7-9"));
        assert!(!is_modern_stateless_protocol("2026-07-28-extra"));
        assert!(!is_modern_stateless_protocol("not-a-version"));
    }

    #[test]
    fn auth_modes_accept_only_the_configured_transport() {
        let token = "1234567890abcdef";
        assert!(auth_matches(AuthMode::Path, token, None, token));
        assert!(!auth_matches(AuthMode::Path, "mcp", Some(token), token));
        assert!(auth_matches(AuthMode::Bearer, "mcp", Some(token), token));
        assert!(!auth_matches(AuthMode::Bearer, token, None, token));
        assert!(auth_matches(AuthMode::Either, token, None, token));
        assert!(auth_matches(AuthMode::Either, "mcp", Some(token), token));
        assert!(!auth_matches(AuthMode::Either, "mcp", Some("wrong"), token));
        assert!(!auth_matches(
            AuthMode::Path,
            "1234567890abcde",
            None,
            token
        ));
        assert!(!auth_matches(
            AuthMode::Bearer,
            "mcp",
            Some("1234567890abcdefx"),
            token
        ));
    }
    #[test]
    fn auth_rejects_wrong_prefix_and_unicode_lookalikes() {
        let token = "1234567890abcdef";
        // A correct-length but wrong-content candidate must fail; so must a
        // longer candidate that merely starts with the token.
        assert!(!token_matches("9234567890abcdef", token));
        assert!(!token_matches(&format!("{token}extra"), token));
        assert!(token_matches(token, token));
        // Empty candidates never authenticate any mode.
        assert!(!auth_matches(AuthMode::Path, "", None, token));
        assert!(!auth_matches(AuthMode::Bearer, "mcp", Some(""), token));
    }
}
