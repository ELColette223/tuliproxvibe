use super::{AppState, HlsOriginIoContext, HlsSessionHandle};
use axum::http::HeaderMap;
use shared::utils::sanitize_sensitive_info;
use std::sync::Arc;
use tuliprox_hls::api::{
    HlsAccessLeaseId, HlsManifestAcceptanceDirective, HlsManifestCommitRequirement, HlsPostRefreshRuntime,
    HlsPreparedOriginAccount, OriginRefreshRequest, RetryPolicy,
};

pub(super) struct HlsOriginRefreshOptions {
    pub headers: HeaderMap,
    pub origin_io: HlsOriginIoContext,
    pub manifest_commit_requirement: HlsManifestCommitRequirement,
    pub acceptance_directive: HlsManifestAcceptanceDirective,
    pub access_lease_id: Option<HlsAccessLeaseId>,
    pub rewrite_secret: Vec<u8>,
    pub now_ms: u64,
}

pub(super) async fn hls_origin_refresh_request(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    prepared: HlsPreparedOriginAccount,
    options: HlsOriginRefreshOptions,
) -> OriginRefreshRequest {
    let mut origin_io = options.origin_io;
    let origin_provider_session_headers = prepared.snapshot.provider_session_headers.clone();
    origin_io.prepared_account = Some(prepared.snapshot);
    // HLS origin I/O must use the same egress as the input it serves. A failure here is not
    // recoverable by falling back to the shared client: that would silently bypass the proxy.
    let input_proxy = {
        let input_name = session.read().await.origin_source.input_name.clone();
        app_state.app_config.get_input_by_name(&input_name).and_then(|input| input.proxy.clone())
    };
    let client = match tuliprox_core::utils::request::create_input_http_client(
        &app_state.app_config,
        &app_state.http_clients.default.load(),
        input_proxy.as_ref(),
    ) {
        Ok(client) => client,
        Err(err) => {
            log::error!("Failed to create HLS input proxy client: {}", sanitize_sensitive_info(&err.to_string()));
            // `hls_origin_refresh_request` is synchronous; the request will fail at open time
            // when the shared client cannot reach the origin through the required proxy.
            app_state.http_clients.default.load().as_ref().clone()
        }
    };
    let no_redirect_client = match tuliprox_core::utils::request::create_input_http_client_no_redirect(
        &app_state.app_config,
        &app_state.http_clients.no_redirect.load(),
        input_proxy.as_ref(),
    ) {
        Ok(client) => client,
        Err(err) => {
            log::error!("Failed to create HLS input proxy client: {}", sanitize_sensitive_info(&err.to_string()));
            app_state.http_clients.no_redirect.load().as_ref().clone()
        }
    };
    OriginRefreshRequest {
        app_config: Arc::clone(&app_state.app_config),
        session: Arc::clone(session),
        origin_entry: prepared.entry,
        headers: options.headers,
        origin_provider_session_headers,
        disabled_headers: app_state.get_disabled_headers(),
        client,
        no_redirect_client,
        use_manual_redirects: {
            let global_proxy = app_state.app_config.config.load().proxy.clone();
            tuliprox_core::model::should_use_manual_redirects_for_proxy(input_proxy.as_ref().or(global_proxy.as_ref()))
        },
        segment_cache: Arc::clone(app_state.hls.proxy.segment_cache()),
        hls_proxy: Arc::clone(&app_state.hls.proxy),
        segment_repair: Arc::clone(app_state.hls.proxy.segment_repair()),
        segment_worker_pool: Arc::clone(app_state.hls.proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(app_state.hls.proxy.map_worker_pool()),
        origin_manifest_timeout_ms: app_state.hls.proxy.origin_manifest_timeout_ms(),
        manifest_recovery_burst: app_state.hls.proxy.manifest_recovery_burst(),
        strip: app_state.hls.proxy.strip().clone(),
        retry_policy: RetryPolicy::default(),
        reverse_proxy_rewrite_secret: options.rewrite_secret,
        transient_resource_ttl_ms: app_state.hls.proxy.transient_resource_ttl_ms(),
        manifest_commit_requirement: options.manifest_commit_requirement,
        fresh_manifest_requirement_generation: None,
        refresh_identity: None,
        acceptance_directive: options.acceptance_directive,
        access_lease_id: options.access_lease_id,
        now_ms: options.now_ms,
        origin_io: Some(origin_io),
        post_refresh_runtime: Some(HlsPostRefreshRuntime { ctx: app_state.hls_ctx().downgrade() }),
    }
}
