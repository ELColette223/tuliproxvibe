use super::ProviderStreamFactoryOptions;
use crate::{
    api::model::StreamError,
    utils::content_coding::{content_decoding_error_from_io, ContentCodingError},
};
use reqwest::{header::HeaderMap, StatusCode};
use shared::{
    model::{ConnectFailureReason, PlaylistItemType},
    utils::Internable,
};
use std::{error::Error as StdError, io};
use tuliprox_session::{response_headers::ProviderResponseHeaderError, stream_ctx::ProviderStreamCtx};
use url::Url;

pub(super) fn record_provider_open_failure(
    ctx: &ProviderStreamCtx,
    stream_options: &ProviderStreamFactoryOptions,
    reason: ConnectFailureReason,
    provider_http_status: Option<StatusCode>,
    provider_error_class: Option<&str>,
) {
    let Some(failure_stage) = stream_options.get_connect_failure_stage() else { return };
    let provider_name =
        stream_options.get_provider().map_or_else(|| "unknown".intern(), |provider| provider.name.clone());
    let Some(info) = stream_options.build_connect_failed_stream_info(provider_name) else { return };
    // Resolve target_name from target_id using the stable target config name.
    let target_name = ctx.app_config.get_target_by_id(info.channel.target_id).as_deref().map(|t| (&t.name).intern());
    ctx.connection_manager.record_connect_failed_with_provider_failure(
        &info,
        reason,
        failure_stage,
        provider_http_status.map(|status| status.as_u16()),
        provider_error_class,
        target_name,
    );
}

pub(super) fn classify_provider_status_error(status: StatusCode) -> &'static str {
    if status.is_client_error() {
        "http_4xx"
    } else if status.is_server_error() {
        "http_5xx"
    } else if status.is_redirection() {
        "http_3xx"
    } else {
        "http_other"
    }
}

/// Provider statuses that mean "this account is at its connection limit".
fn bandwidth_limit_exceeded_status() -> StatusCode {
    StatusCode::from_u16(509).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
}

pub(super) fn is_account_full_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::SERVICE_UNAVAILABLE
        || status == bandwidth_limit_exceeded_status()
}

/// Some panels answer a full account with an HTTP 200 and a human-readable
/// "max connections" page. The body text is the only signal in that case.
pub(super) fn looks_like_connection_limit_message(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const MARKERS: [&str; 8] = [
        "max connections",
        "maximum connections",
        "connection limit",
        "connections limit",
        "too many connections",
        "active_cons",
        "no connections available",
        "device limit",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// The low-level HTTP helpers turn non-success statuses into `io::Error`
/// (`Request failed (429 Too Many Requests): ...`). Recover the status so
/// `rotate_on_full` can treat account-full responses as `AccountFull`
/// instead of a generic channel-unavailable failure.
pub(super) fn account_full_status_from_io_error(err: &std::io::Error) -> Option<StatusCode> {
    let message = err.to_string();
    let Some(rest) = message.strip_prefix("Request failed (") else {
        return None;
    };
    let code_token = rest.split([' ', ')']).next()?;
    let code = code_token.parse::<u16>().ok()?;
    let status = StatusCode::from_u16(code).ok()?;
    is_account_full_status(status).then_some(status)
}

fn provider_content_type_looks_like_html(headers: &HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case("text/html"))
}

pub(super) fn should_reject_success_response_content_type(item_type: PlaylistItemType, headers: &HeaderMap) -> bool {
    !item_type.is_live_adaptive() && provider_content_type_looks_like_html(headers)
}

#[derive(Debug)]
pub(super) enum ProviderStreamRequestFailure {
    HlsStatus { status: StatusCode, headers: Vec<(String, String)> },
    Status { status: StatusCode, provider_error_class: &'static str, serve_channel_unavailable: bool },
    /// The provider rejected the open because the account is at its connection limit.
    /// Kept distinct so `rotate_on_full` can retry on another account instead of
    /// serving a channel-unavailable response.
    AccountFull { status: StatusCode, provider_error_class: &'static str },
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ProviderStreamPreparationError {
    #[error(transparent)]
    ContentCoding(#[from] ContentCodingError),

    #[error(transparent)]
    ResponseHeader(#[from] ProviderResponseHeaderError),

    #[error("provider response head is unavailable for status {status} or Content-Range={has_content_range}")]
    DeferredResponseHead { status: StatusCode, has_content_range: bool },
}

impl ProviderStreamPreparationError {
    pub(super) fn provider_error_class(&self) -> &'static str {
        match self {
            Self::ContentCoding(ContentCodingError::InvalidHeader | ContentCodingError::Unsupported(_))
            | Self::ResponseHeader(ProviderResponseHeaderError::InvalidContentEncoding) => "content_encoding",
            Self::ContentCoding(ContentCodingError::EncodedPartialContent) => "encoded_partial_content",
            Self::ContentCoding(ContentCodingError::PrefixRead(_)) => "body",
            Self::ResponseHeader(
                ProviderResponseHeaderError::InvalidContentLength | ProviderResponseHeaderError::InvalidContentRange,
            )
            | Self::DeferredResponseHead { .. } => "response_headers",
        }
    }
}

impl ProviderStreamRequestFailure {
    pub(super) fn status(&self) -> StatusCode {
        match self {
            Self::Status { status, .. } | Self::HlsStatus { status, .. } | Self::AccountFull { status, .. } => *status,
        }
    }

    pub(super) fn provider_error_class(&self) -> &'static str {
        match self {
            Self::Status { provider_error_class, .. } | Self::AccountFull { provider_error_class, .. } => {
                provider_error_class
            }
            Self::HlsStatus { status, .. } => classify_provider_status_error(*status),
        }
    }

    pub(super) fn should_serve_channel_unavailable(&self) -> bool {
        match self {
            Self::Status { serve_channel_unavailable, .. } => *serve_channel_unavailable,
            Self::AccountFull { .. } => true,
            Self::HlsStatus { .. } => false,
        }
    }

    pub(super) fn is_account_full(&self) -> bool { matches!(self, Self::AccountFull { .. }) }
}

pub(super) fn classify_provider_io_error(err: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;

    match err.kind() {
        ErrorKind::TimedOut => "timeout",
        ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected => "connect",
        ErrorKind::AddrNotAvailable => "dns",
        _ => {
            let lowered = err.to_string().to_ascii_lowercase();
            if lowered.contains("dns")
                || lowered.contains("failed to lookup address information")
                || lowered.contains("name or service not known")
                || lowered.contains("no such host")
                || lowered.contains("temporary failure in name resolution")
            {
                "dns"
            } else {
                "io"
            }
        }
    }
}

pub(super) fn provider_decoded_body_error(error: &io::Error) -> StreamError {
    if let Some(error) = content_decoding_error_from_io(error) {
        return StreamError::ContentDecoding(format!("coding={}", error.coding.as_http_token()));
    }

    let mut source = StdError::source(error);
    while let Some(current) = source {
        if let Some(error) = current.downcast_ref::<reqwest::Error>() {
            return StreamError::reqwest(error);
        }
        source = current.source();
    }
    StreamError::StdIo(error.to_string())
}

pub(super) fn failed_stream_account(
    input: &crate::model::ConfigInput,
    options: &ProviderStreamFactoryOptions,
) -> Option<crate::model::ConfigInput> {
    let url = &options.url;
    let credentials = shared::utils::get_credentials_from_url(url);
    let segments: Vec<_> = url.path_segments().into_iter().flatten().collect();
    let matches = |username: Option<&str>, password: Option<&str>| {
        username.zip(password).is_some_and(|(username, password)| {
            credentials.0.as_deref() == Some(username) && credentials.1.as_deref() == Some(password)
                || segments.windows(2).any(|pair| pair[0] == username && pair[1] == password)
        })
    };
    let matches_origin = |base: &str| {
        input
            .resolve_url(base)
            .ok()
            .and_then(|base| Url::parse(&base).ok())
            .is_some_and(|base| base.origin() == url.origin())
    };
    let account = if let Some(allocated) = &options.account {
        if input.name == allocated.name {
            input.clone()
        } else {
            let alias = input.aliases.iter().flatten().find(|alias| alias.name == allocated.name)?;
            input.as_input(alias)
        }
    } else if matches(input.username.as_deref(), input.password.as_deref()) && matches_origin(&input.url) {
        input.clone()
    } else {
        let alias = input.aliases.iter().flatten().find(|alias| {
            matches(alias.username.as_deref(), alias.password.as_deref()) && matches_origin(&alias.url)
        })?;
        input.as_input(alias)
    };
    if options.account.as_ref().is_some_and(|allocated| account.account_identity() != allocated.account_identity()) {
        return None;
    }
    Some(account)
}
