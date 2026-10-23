use super::{
    error::{record_provider_open_failure, ProviderStreamRequestFailure},
    options::should_wrap_provider_stream_in_buffer,
    request::get_provider_stream,
    ProviderOpenGuard, ProviderStreamFactoryFlags, ProviderStreamFactoryOptions, ProviderStreamOpenLifecycle,
    LIVE_MPEGTS_BODY_IDLE, LIVE_MPEGTS_MAX_EMPTY_EPISODES, LIVE_MPEGTS_RECONNECT_MIN_INTERVAL,
};
use crate::api::model::{
    create_channel_unavailable_stream, get_response_headers, streams::client_stream::ClientStream, BoxedProviderStream,
    CustomVideoStreamType, ProviderStreamFactoryResponse,
};
use futures::{stream, Future, StreamExt};
use log::warn;
use reqwest::StatusCode;
use shared::{model::ConnectFailureReason, utils::sanitize_sensitive_info};
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

/// What the reconnect decided for the episode that just ended.
pub(super) enum EpisodePlan {
    /// Another upstream body is ready; its bytes continue on the same client response.
    ///
    /// `empty_episodes` carries the running count of consecutive episodes without data, so the
    /// caller can persist it and the limit is actually reached.
    Reopen { body: BoxedProviderStream, opened_at: TokioInstant, empty_episodes: u32 },
    /// No further episode can serve the viewer.
    Stop,
}

/// Opens the next episode of a live MPEG-TS stream.
///
/// `empty_episodes` counts consecutive episodes that produced no byte, including this one. The
/// caller resets it on the first byte received, so a provider that legitimately ends bodies after
/// some megabytes keeps reconnecting, while one that answers with nothing gives up.
pub(super) async fn next_mpegts_episode<F, Fut>(
    options: &ProviderStreamFactoryOptions,
    reopen: &mut F,
    empty_episodes: u32,
    opened_at: TokioInstant,
) -> EpisodePlan
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Option<BoxedProviderStream>> + Send + 'static,
{
    if !options.should_continue() {
        return EpisodePlan::Stop;
    }
    let empty_episodes = empty_episodes.saturating_add(1);
    if empty_episodes > LIVE_MPEGTS_MAX_EMPTY_EPISODES {
        warn!(
            "Live MPEG-TS reconnect gave up after {empty_episodes} episodes without data for {}",
            sanitize_sensitive_info(options.get_url_as_str())
        );
        return EpisodePlan::Stop;
    }

    // A provider that closes bodies immediately must not turn this into a request flood. The
    // interval is measured from the previous open, so episodes that delivered data do not wait.
    let since_open = TokioInstant::now().saturating_duration_since(opened_at);
    if since_open < LIVE_MPEGTS_RECONNECT_MIN_INTERVAL {
        let wait = LIVE_MPEGTS_RECONNECT_MIN_INTERVAL - since_open;
        tokio::select! {
            biased;
            () = options.reconnect_flag.cancelled() => return EpisodePlan::Stop,
            () = tokio::time::sleep(wait) => {},
        }
    }

    match reopen().await {
        Some(body) if options.should_continue() => {
            EpisodePlan::Reopen { body, opened_at: TokioInstant::now(), empty_episodes }
        }
        _ => EpisodePlan::Stop,
    }
}

/// A live MPEG-TS body is reopened when the provider closes it.
///
/// Many panels answer a channel with a body that ends after a few megabytes and serve the same
/// channel on a fresh request. The viewer is still watching and still expects bytes, so ending the
/// client response at that first EOF drops a working stream. This wrapper treats the upstream body
/// as a sequence of episodes: when one ends, the next request is opened and its bytes continue on
/// the same downstream response.
///
/// Reopening is bounded by two things the caller can trust:
/// - the client's `reconnect_flag` — once the viewer leaves there is nobody to serve;
/// - `LIVE_MPEGTS_MAX_EMPTY_EPISODES` consecutive episodes that produced no byte, so a channel that
///   never comes back ends instead of looping.
///
/// `reopen` receives the same options the first open used (URL already selected for the account,
/// headers, laterals), so a reconnect stays on the same account and does not re-resolve anything.
pub(super) fn enforce_live_mpegts_body_continuity<F, Fut>(
    initial: BoxedProviderStream,
    options: ProviderStreamFactoryOptions,
    reopen: F,
) -> BoxedProviderStream
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Option<BoxedProviderStream>> + Send + 'static,
{
    stream::unfold(
        (
            initial,
            options,
            TokioInstant::now() + LIVE_MPEGTS_BODY_IDLE,
            reopen,
            0_u32,
            TokioInstant::now(),
        ),
        |(mut body, options, mut idle_deadline, mut reopen, mut empty_episodes, mut opened_at)| async move {
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
                        idle_deadline = TokioInstant::now() + LIVE_MPEGTS_BODY_IDLE;
                        return Some((Ok(bytes), (body, options, idle_deadline, reopen, 0, opened_at)));
                    }
                    Ok(Some(Ok(_))) => {
                        if TokioInstant::now() >= idle_deadline {
                            warn!("Live MPEG-TS upstream body idle timeout; ending stream");
                            return Some((
                                Err(StreamError::ProviderIdleTimeout),
                                (body, options, idle_deadline, reopen, empty_episodes, opened_at),
                            ));
                        }
                        tokio::select! {
                            biased;
                            () = options.reconnect_flag.cancelled() => return None,
                            () = tokio::time::sleep_until(idle_deadline) => {},
                            () = tokio::time::sleep(Duration::from_millis(5)) => {},
                        }
                    }
                    Ok(Some(Err(error))) => {
                        // A transport error mid-body means this episode is over; the reconnect
                        // below decides whether another one can serve the viewer.
                        debug_if_enabled!("Live MPEG-TS upstream body error; reopening");
                        warn!("Live MPEG-TS upstream body error ({error}); reopening");
                        match next_mpegts_episode(&options, &mut reopen, empty_episodes, opened_at).await {
                            EpisodePlan::Reopen { body: next, opened_at: next_opened, empty_episodes: next_empty } => {
                                body = next;
                                idle_deadline = TokioInstant::now() + LIVE_MPEGTS_BODY_IDLE;
                                opened_at = next_opened;
                                empty_episodes = next_empty;
                                // Continue polling the new body in this same unfold step: emitting an
                                // empty chunk would surface a zero-length read to the viewer.
                                continue;
                            }
                            EpisodePlan::Stop => {
                                return Some((
                                    Err(error),
                                    (body, options, idle_deadline, reopen, empty_episodes, opened_at),
                                ));
                            }
                        }
                    }
                    Ok(None) => {
                        debug_if_enabled!("Live MPEG-TS upstream body ended; reopening");
                        match next_mpegts_episode(&options, &mut reopen, empty_episodes, opened_at).await {
                            EpisodePlan::Reopen { body: next, opened_at: next_opened, empty_episodes: next_empty } => {
                                body = next;
                                idle_deadline = TokioInstant::now() + LIVE_MPEGTS_BODY_IDLE;
                                opened_at = next_opened;
                                empty_episodes = next_empty;
                                // Same as the error branch: keep polling so no empty chunk is emitted.
                                continue;
                            }
                            EpisodePlan::Stop => {
                                debug_if_enabled!("Live MPEG-TS reconnect gave up; closing stream");
                                return None;
                            }
                        }
                    }
                    Err(_) => {
                        warn!("Live MPEG-TS upstream body idle timeout; ending stream");
                        return Some((
                            Err(StreamError::ProviderIdleTimeout),
                            (body, options, idle_deadline, reopen, empty_episodes, opened_at),
                        ));
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
    /// The panel has no such channel for this account. Another account of the same input may
    /// have it, so the caller retries there before giving up on the stream.
    ChannelMissing {
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
            let init_stream = if stream_options.live_mpegts_recovery_enabled(&info) {
                // Reopen with the same options: the URL is already the one selected for this
                // account, so a reconnect stays on it instead of re-resolving laterals.
                let reopen_ctx = ctx.clone();
                let reopen_client = client.clone();
                let reopen_options = stream_options.clone();
                enforce_live_mpegts_body_continuity(init_stream, stream_options.clone(), move || {
                    let ctx = reopen_ctx.clone();
                    let client = reopen_client.clone();
                    let options = reopen_options.clone();
                    async move {
                        match get_provider_stream(&ctx, &client, &options).await {
                            Ok(Some(ProviderStreamFactoryResponse { stream, .. })) => Some(stream),
                            _ => None,
                        }
                    }
                })
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
            if failure.is_channel_missing() {
                // The channel name is valid at this panel but absent for this account; another
                // account of the same input may still carry it.
                return ProviderStreamOpenOutcome::ChannelMissing { status, provider_error_class };
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
        ProviderStreamOpenOutcome::AccountFull { .. } | ProviderStreamOpenOutcome::ChannelMissing { .. } => {
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
