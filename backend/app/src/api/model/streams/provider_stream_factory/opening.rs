use super::{
    error::{record_provider_open_failure, ProviderStreamRequestFailure},
    options::should_wrap_provider_stream_in_buffer,
    request::get_provider_stream,
    ProviderOpenGuard, ProviderStreamFactoryFlags, ProviderStreamFactoryOptions, ProviderStreamOpenLifecycle,
    SHARED_MPEGTS_BODY_IDLE,
};
use crate::api::model::{
    create_channel_unavailable_stream, get_response_headers, streams::client_stream::ClientStream, BoxedProviderStream,
    CustomVideoStreamType, ProviderStreamFactoryResponse,
};
use futures::{stream, StreamExt};
use log::warn;
use reqwest::StatusCode;
use shared::model::ConnectFailureReason;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;
use tuliprox_core::{model::StreamError, utils::debug_if_enabled};
use tuliprox_session::{
    stream_ctx::ProviderStreamCtx,
    streams::{ProviderBodyOwner, ProviderBodyOwnerConfig},
    ManagedProviderHandle, ProviderSessionHeaders,
};

/// Result of opening a provider stream.
pub enum ProviderStreamOpen {
    Stream(ProviderStreamFactoryResponse),
    /// A finite HLS resource keeps the upstream error status and its range and retry headers
    /// instead of receiving a fallback body.
    UpstreamStatus {
        status: StatusCode,
        headers: Vec<(String, String)>,
    },
}

/// Shared live MPEG-TS bodies must not be reopened mid-response: a new upstream HTTP body
/// cannot be proven to continue the same transport stream, so the origin ends and the next
/// client request opens a fresh response.
fn enforce_shared_mpegts_body_continuity(
    initial: BoxedProviderStream,
    options: ProviderStreamFactoryOptions,
) -> BoxedProviderStream {
    stream::unfold(
        (initial, options, TokioInstant::now() + SHARED_MPEGTS_BODY_IDLE),
        |(mut body, options, mut idle_deadline)| async move {
            if !options.should_continue() {
                return None;
            }
            loop {
                let chunk = tokio::select! {
                    biased;
                    () = options.reconnect_flag.cancelled() => return None,
                    result = tokio::time::timeout_at(idle_deadline, body.next()) => result,
                };
                match chunk {
                    Ok(Some(Ok(bytes))) if !bytes.is_empty() => {
                        // Media progress pushes the idle deadline forward; only a genuinely
                        // silent upstream reaches it.
                        idle_deadline = TokioInstant::now() + SHARED_MPEGTS_BODY_IDLE;
                        return Some((Ok(bytes), (body, options, idle_deadline)));
                    }
                    Ok(Some(Ok(_))) => {
                        if TokioInstant::now() >= idle_deadline {
                            warn!("Shared MPEG-TS upstream body idle timeout; ending origin");
                            return Some((Err(StreamError::ProviderIdleTimeout), (body, options, idle_deadline)));
                        }
                        tokio::select! {
                            biased;
                            () = options.reconnect_flag.cancelled() => return None,
                            () = tokio::time::sleep_until(idle_deadline) => {},
                            () = tokio::time::sleep(Duration::from_millis(5)) => {},
                        }
                    }
                    Ok(Some(Err(error))) => {
                        warn!("Shared MPEG-TS upstream body error; ending origin");
                        return Some((Err(error), (body, options, idle_deadline)));
                    }
                    Ok(None) => {
                        debug_if_enabled!("Shared MPEG-TS upstream body ended; closing origin");
                        return None;
                    }
                    Err(_) => {
                        warn!("Shared MPEG-TS upstream body idle timeout; ending origin");
                        return Some((Err(StreamError::ProviderIdleTimeout), (body, options, idle_deadline)));
                    }
                }
            }
        },
    )
    .boxed()
}

/// Typed provider-open outcome for callers that need to rotate on an account-full
/// rejection before it is turned into a channel-unavailable response.
pub enum ProviderStreamOpenOutcome {
    Opened(ProviderStreamFactoryResponse),
    /// A finite HLS resource keeps the upstream error status and its range and retry headers.
    UpstreamStatus {
        status: StatusCode,
        headers: Vec<(String, String)>,
    },
    AccountFull {
        status: StatusCode,
        provider_error_class: &'static str,
    },
    Failed,
}

#[allow(clippy::too_many_lines)]
pub async fn create_provider_stream_outcome(
    ctx: &ProviderStreamCtx,
    client: &reqwest::Client,
    stream_options: ProviderStreamFactoryOptions,
) -> ProviderStreamOpenOutcome {
    let mut open_guard = ProviderOpenGuard { token: stream_options.get_completion_token(), handed_off: false };
    match get_provider_stream(ctx, client, &stream_options).await {
        Ok(Some(ProviderStreamFactoryResponse { stream: init_stream, info, provider_session_headers, .. })) => {
            let continuity_guarded = stream_options.shared_mpegts_recovery_enabled(&info);
            let init_stream = if continuity_guarded {
                enforce_shared_mpegts_body_continuity(init_stream, stream_options.clone())
            } else {
                init_stream
            };
            let continue_signal = stream_options.get_reconnect_flag_clone();
            if let Some((_headers, _status, _response_url, Some(custom_video_type))) = &info {
                let reason = match custom_video_type {
                    CustomVideoStreamType::ChannelUnavailable => Some(ConnectFailureReason::ChannelUnavailable),
                    CustomVideoStreamType::Provisioning => Some(ConnectFailureReason::Provisioning),
                    CustomVideoStreamType::ProviderConnectionsExhausted => {
                        Some(ConnectFailureReason::ProviderConnectionsExhausted)
                    }
                    _ => None,
                };
                if let Some(reason) = reason {
                    record_provider_open_failure(ctx, &stream_options, reason, None, None);
                }
                return ProviderStreamOpenOutcome::Opened(ProviderStreamFactoryResponse {
                    stream: ClientStream::new(init_stream, continue_signal, None, stream_options.get_url_as_str())
                        .boxed(),
                    info,
                    provider_session_headers,
                    has_upstream_owner: false,
                });
            }
            let handle_cancel = stream_options.get_cancel_token();
            let completion_token = stream_options.get_completion_token();
            let close_reason = stream_options.get_close_reason();

            let cancel_token = if let Some(hc) = handle_cancel {
                let combined = CancellationToken::new();
                let c1 = combined.clone();
                let c2 = combined.clone();
                let hc_clone = hc.clone();
                let cs_clone = continue_signal.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        () = hc_clone.cancelled() => {
                            cs_clone.cancel();
                            c1.cancel();
                        }
                        () = cs_clone.cancelled() => {
                            c2.cancel();
                        }
                    }
                });
                combined
            } else {
                continue_signal.clone()
            };

            let body_owner_config = if should_wrap_provider_stream_in_buffer(&stream_options) {
                ProviderBodyOwnerConfig::for_buffered(
                    stream_options.get_buffer_size(),
                    stream_options.get_buffer_max_bytes(),
                )
            } else {
                ProviderBodyOwnerConfig::for_direct_body()
            };

            open_guard.handed_off = true;
            let stream = ProviderBodyOwner::new(
                init_stream.boxed(),
                body_owner_config,
                cancel_token,
                completion_token,
                close_reason,
            )
            .boxed();

            ProviderStreamOpenOutcome::Opened(ProviderStreamFactoryResponse {
                stream: ClientStream::new(stream, continue_signal.clone(), None, stream_options.get_url_as_str())
                    .boxed(),
                info,
                provider_session_headers,
                has_upstream_owner: true,
            })
        }
        Ok(None) => ProviderStreamOpenOutcome::Failed,
        Err(failure) => {
            let status = failure.status();
            let provider_error_class = failure.provider_error_class();
            if stream_options.flags.contains(ProviderStreamFactoryFlags::HlsResource) {
                let headers = match failure {
                    ProviderStreamRequestFailure::HlsStatus { headers, .. } => headers,
                    _ => Vec::new(),
                };
                return ProviderStreamOpenOutcome::UpstreamStatus { status, headers };
            }
            if failure.is_account_full() {
                record_provider_open_failure(
                    ctx,
                    &stream_options,
                    ConnectFailureReason::ProviderConnectionsExhausted,
                    Some(status),
                    Some(provider_error_class),
                );
                return ProviderStreamOpenOutcome::AccountFull { status, provider_error_class };
            }
            record_provider_open_failure(
                ctx,
                &stream_options,
                ConnectFailureReason::ChannelUnavailable,
                Some(status),
                Some(provider_error_class),
            );
            if let (Some(boxed_provider_stream), response_info) = create_channel_unavailable_stream(
                &ctx.app_config,
                &get_response_headers(stream_options.get_headers()),
                StatusCode::OK,
            ) {
                return ProviderStreamOpenOutcome::Opened(ProviderStreamFactoryResponse {
                    stream: boxed_provider_stream,
                    info: response_info,
                    provider_session_headers: ProviderSessionHeaders::default(),
                    has_upstream_owner: false,
                });
            }
            ProviderStreamOpenOutcome::Failed
        }
    }
}

/// Converts the typed outcome back into the legacy optional stream, serving a
/// channel-unavailable body when the account is full.
#[allow(clippy::too_many_lines)]
pub async fn create_provider_stream(
    ctx: &ProviderStreamCtx,
    client: &reqwest::Client,
    stream_options: ProviderStreamFactoryOptions,
) -> Option<ProviderStreamOpen> {
    let response_headers = get_response_headers(stream_options.get_headers());
    match create_provider_stream_outcome(ctx, client, stream_options).await {
        ProviderStreamOpenOutcome::Opened(response) => Some(ProviderStreamOpen::Stream(response)),
        ProviderStreamOpenOutcome::UpstreamStatus { status, headers } => {
            Some(ProviderStreamOpen::UpstreamStatus { status, headers })
        }
        ProviderStreamOpenOutcome::AccountFull { .. } => {
            let (boxed_provider_stream, response_info) =
                create_channel_unavailable_stream(&ctx.app_config, &response_headers, StatusCode::OK);
            boxed_provider_stream.map(|stream| {
                ProviderStreamOpen::Stream(ProviderStreamFactoryResponse {
                    stream,
                    info: response_info,
                    provider_session_headers: ProviderSessionHeaders::default(),
                    has_upstream_owner: false,
                })
            })
        }
        ProviderStreamOpenOutcome::Failed => None,
    }
}

impl ProviderStreamOpenLifecycle {
    pub fn from_managed(managed: &ManagedProviderHandle) -> Option<Self> {
        let handle = managed.handle()?;
        Some(Self {
            manager: Arc::clone(managed.manager()),
            allocation_id: handle.allocation_id,
            cancel_token: handle.cancel_token.clone(),
            completion_token: handle.completion_token.clone(),
            close_reason: Some(Arc::clone(&handle.close_reason)),
            account: handle.allocation.get_provider_config(),
        })
    }

    pub fn mark_opening(&self) { self.manager.mark_opening(self.allocation_id); }

    pub fn register_body_owner(&self) -> bool { self.manager.register_body_owner(self.allocation_id) }
}

pub async fn open_provider_stream_with_lifecycle(
    ctx: &ProviderStreamCtx,
    client: &reqwest::Client,
    mut stream_options: ProviderStreamFactoryOptions,
    lifecycle: Option<ProviderStreamOpenLifecycle>,
) -> Option<ProviderStreamOpen> {
    if let Some(ref lc) = lifecycle {
        lc.mark_opening();
        stream_options.account.clone_from(&lc.account);
        stream_options.set_provider_handle_tokens(
            lc.cancel_token.clone(),
            lc.completion_token.clone(),
            lc.close_reason.clone(),
        );
    }
    let open = create_provider_stream(ctx, client, stream_options).await?;
    if let (ProviderStreamOpen::Stream(response), Some(lc)) = (&open, lifecycle.as_ref()) {
        if response.has_upstream_owner && !lc.register_body_owner() {
            return None;
        }
    }
    Some(open)
}

/// Same as [`open_provider_stream_with_lifecycle`], but preserves the account-full outcome so
/// `rotate_on_full` can retry on another account instead of serving a channel-unavailable body.
pub async fn open_provider_stream_outcome_with_lifecycle(
    ctx: &ProviderStreamCtx,
    client: &reqwest::Client,
    mut stream_options: ProviderStreamFactoryOptions,
    lifecycle: Option<ProviderStreamOpenLifecycle>,
) -> ProviderStreamOpenOutcome {
    if let Some(ref lc) = lifecycle {
        lc.mark_opening();
        stream_options.account.clone_from(&lc.account);
        stream_options.set_provider_handle_tokens(
            lc.cancel_token.clone(),
            lc.completion_token.clone(),
            lc.close_reason.clone(),
        );
    }
    let outcome = create_provider_stream_outcome(ctx, client, stream_options).await;
    if let (ProviderStreamOpenOutcome::Opened(response), Some(lc)) = (&outcome, lifecycle.as_ref()) {
        if response.has_upstream_owner && !lc.register_body_owner() {
            return ProviderStreamOpenOutcome::Failed;
        }
    }
    outcome
}
