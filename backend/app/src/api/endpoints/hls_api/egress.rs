//! Optional input-proxy egress for HLS playback.
//!
//! The normal playback path stays direct. A failed direct request can use the input's proxy when
//! the input explicitly enables `proxy_stream_fallback`. The proxy client never follows redirects
//! internally because reqwest drops proxy credentials on an automatic cross-origin redirect.

use crate::{api::model::AppState, model::InputSource};
use log::warn;
use std::sync::Arc;

pub(in crate::api::endpoints::hls_api) fn hls_proxy_client(
    app_state: &Arc<AppState>,
    input: &InputSource,
) -> Option<reqwest::Client> {
    let proxy = input
        .proxy
        .as_ref()
        .filter(|_| input.proxy_stream_fallback || input.proxy_stream_first)?;
    match crate::utils::request::create_input_http_client_with_redirect(
        &app_state.app_config,
        app_state.http_clients.stream_no_redirect.load().as_ref(),
        Some(proxy),
        reqwest::redirect::Policy::none(),
    ) {
        Ok(client) => Some(client),
        Err(err) => {
            warn!("Cannot build the HLS proxy fallback client: {}", crate::utils::sanitize_sensitive_info(&err.to_string()));
            None
        }
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_proxy_fallback_candidate(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    let message = err.to_string();
    if let Some(rest) = message.strip_prefix("Request failed (") {
        let status = rest.split([' ', ')']).next().and_then(|token| token.parse::<u16>().ok());
        // 404 is eligible because this panel answers a firewalled client with 404; the other
        // egress proves whether the resource is really absent. 503 stays excluded: this panel
        // uses it for a full account, and switching egress cannot add capacity.
        return matches!(status, Some(404 | 408 | 500..=502 | 504..=508 | 510..=599));
    }
    match err.kind() {
        ErrorKind::TimedOut
        | ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected
        | ErrorKind::AddrNotAvailable => true,
        // The request engine uses Other for transport errors, but so do invalid redirects,
        // content decoding and decoded-body limits. Only its request-level error labels qualify.
        ErrorKind::Other => message.starts_with("Request error:") || message == "All attempts and providers exhausted",
        _ => false,
    }
}
