//! Playback egress for HLS origin traffic.
//!
//! HLS manifests and segments are playback, exactly like MPEG-TS bodies, so they connect directly
//! by default. An input may opt into using its own proxy when a direct connection cannot reach the
//! provider, mirroring `options.proxy_stream_fallback` on the MPEG-TS path.
//!
//! `reqwest` drops `Proxy-Authorization` when it follows a redirect internally, so a proxied
//! pair is built with `Policy::none()` on both sides and the caller walks redirects one request
//! per hop.

use crate::{api::model::AppState, model::InputSource};
use log::warn;
use std::sync::Arc;

/// The clients one HLS playback request must use for its origin hop.
pub(in crate::api::endpoints::hls_api) struct HlsPlaybackEgress {
    /// Client allowed to follow redirects itself.
    pub client: reqwest::Client,
    /// Client that never follows redirects, for callers that walk the redirect chain.
    pub no_redirect_client: reqwest::Client,
    /// Whether the caller must walk redirects itself.
    pub manual_redirects: bool,
}

impl HlsPlaybackEgress {
    /// Resolves the egress for `input`, using the input proxy only when the input opted in.
    ///
    /// Inputs that did not opt in get the shared direct clients unchanged, so default HLS
    /// playback behaviour is untouched.
    pub(in crate::api::endpoints::hls_api) fn for_input(
        app_state: &Arc<AppState>,
        input: Option<&InputSource>,
    ) -> Self {
        let proxy = input
            .filter(|input| input.proxy_stream_fallback)
            .and_then(|input| input.proxy.as_ref());
        if let Some(proxy) = proxy {
            let build = |policy| {
                crate::utils::request::create_input_http_client_with_redirect(
                    &app_state.app_config,
                    app_state.http_clients.stream_no_redirect.load().as_ref(),
                    Some(proxy),
                    policy,
                )
            };
            match (build(reqwest::redirect::Policy::none()), build(reqwest::redirect::Policy::limited(10))) {
                (Ok(no_redirect_client), Ok(client)) => {
                    return Self { client, no_redirect_client, manual_redirects: true };
                }
                (Err(err), _) | (_, Err(err)) => warn!(
                    "Cannot build the HLS proxy fallback client; keeping the direct connection: {}",
                    crate::utils::sanitize_sensitive_info(&err.to_string())
                ),
            }
        }
        Self {
            client: app_state.http_clients.stream.load().as_ref().clone(),
            no_redirect_client: app_state.http_clients.stream_no_redirect.load().as_ref().clone(),
            manual_redirects: false,
        }
    }
}
