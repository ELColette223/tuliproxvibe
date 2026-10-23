use super::*;
use crate::api::model::streams::provider_stream_factory::opening::{
    enforce_live_mpegts_body_continuity, next_mpegts_episode, EpisodePlan,
};
use crate::api::model::{ProviderContentRepresentationMode, ProviderStreamInfo};
use bytes::Bytes;
use futures::{stream, StreamExt};
use shared::model::PlaylistItemType;
use std::{sync::atomic::{AtomicUsize, Ordering}, sync::Arc};
use tokio::{sync::Mutex as TokioMutex, time::Instant as TokioInstant};

fn live_options(share_stream: bool, item_type: PlaylistItemType) -> ProviderStreamFactoryOptions {
    let stream_url = Url::parse("http://origin.example/live/1.ts").expect("url");
    let req_headers = HeaderMap::new();
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: false,
        buffer_size: 0,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::Stream,
    };
    ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr: "127.0.0.1:8080".parse().unwrap(),
        item_type,
        share_stream,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    })
}

fn live_info(content_type: &str, extra: &[(&str, &str)]) -> ProviderStreamInfo {
    let mut headers = vec![("content-type".to_string(), content_type.to_string())];
    headers.extend(extra.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())));
    Some((headers, StatusCode::OK, None, None))
}

fn body_of(chunks: Vec<&'static [u8]>) -> BoxedProviderStream {
    let items: Vec<Result<Bytes, StreamError>> =
        chunks.into_iter().map(|chunk| Ok(Bytes::from_static(chunk))).collect();
    stream::iter(items).boxed()
}

/// The response shape decides whether a body may be reopened.
#[test]
fn only_plain_live_mpegts_is_reopened() {
    let live = live_options(false, PlaylistItemType::Live);
    assert!(live.live_mpegts_recovery_enabled(&live_info("video/mp2t", &[])), "a plain live TS body is reopenable");

    let vod = live_options(false, PlaylistItemType::Video);
    assert!(
        !vod.live_mpegts_recovery_enabled(&live_info("video/mp2t", &[])),
        "VOD must keep its single body: a reopen would restart the file"
    );

    let hls = live_options(false, PlaylistItemType::LiveHls);
    assert!(!hls.live_mpegts_recovery_enabled(&live_info("video/mp2t", &[])), "HLS has its own resource model");

    assert!(
        !live.live_mpegts_recovery_enabled(&live_info("video/mp2t", &[("content-length", "1024")])),
        "a body with a known length is not a live stream"
    );
    assert!(
        !live.live_mpegts_recovery_enabled(&live_info("video/mp2t", &[("content-encoding", "gzip")])),
        "an encoded body cannot be continued mid-stream"
    );
    assert!(
        !live.live_mpegts_recovery_enabled(&live_info("text/html", &[])),
        "an error page is not a channel"
    );
}

/// The reported failure: the provider ends the body, the viewer keeps watching, bytes must continue.
#[tokio::test]
async fn an_ended_body_is_reopened_and_bytes_continue() {
    let options = live_options(false, PlaylistItemType::Live);
    let reopens = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reopens);
    let body_slot = Arc::new(TokioMutex::new(Some(body_of(vec![b"second-episode"]))));

    let stream = enforce_live_mpegts_body_continuity(
        body_of(vec![b"first-episode"]),
        options,
        move || {
            let counter = Arc::clone(&counter);
            let body = Arc::clone(&body_slot);
            async move {
                // Only the first reopen produces data; after that the provider has nothing, which
                // is what ends the stream instead of looping.
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    let mut slot = body.lock().await;
                    slot.take()
                } else {
                    None
                }
            }
        },
    );

    let collected: Vec<u8> =
        stream.filter_map(|item| async move { item.ok() }).flat_map(|bytes| stream::iter(bytes.to_vec())).collect().await;

    assert!(collected.windows(13).any(|window| window == b"first-episode"), "the first episode survives");
    assert!(collected.windows(14).any(|window| window == b"second-episode"), "the reopened episode is delivered");
    assert!(
        reopens.load(Ordering::SeqCst) >= 1,
        "the ended body must trigger a reopen attempt, got {}",
        reopens.load(Ordering::SeqCst)
    );
}

/// A viewer who left must not keep a provider request alive.
#[tokio::test]
async fn a_disconnected_client_does_not_reopen() {
    let options = live_options(false, PlaylistItemType::Live);
    options.cancel_reconnect();
    let reopens = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reopens);

    let stream =
        enforce_live_mpegts_body_continuity(body_of(vec![b"only"]), options, move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(body_of(vec![b"unexpected"]))
            }
        });

    let collected: Vec<Result<Bytes, StreamError>> = stream.collect().await;

    assert!(collected.iter().all(Result::is_ok), "no error is invented for the viewer");
    assert_eq!(reopens.load(Ordering::SeqCst), 0, "a closed client must not trigger a provider request");
}

/// An account that never comes back must end, not loop forever.
#[tokio::test]
async fn consecutive_empty_episodes_eventually_give_up() {
    let options = live_options(false, PlaylistItemType::Live);
    let reopens = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reopens);

    let stream = enforce_live_mpegts_body_continuity(
        body_of(vec![]),
        options,
        move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(body_of(vec![]))
            }
        },
    );

    let collected: Vec<Result<Bytes, StreamError>> = stream.collect().await;
    let attempts = reopens.load(Ordering::SeqCst);

    assert!(collected.is_empty(), "no data means no bytes to the viewer");
    assert!(
        attempts <= usize::try_from(LIVE_MPEGTS_MAX_EMPTY_EPISODES).expect("limit fits"),
        "the reconnect must stop after {LIVE_MPEGTS_MAX_EMPTY_EPISODES} empty episodes, got {attempts}"
    );
}

/// A silent upstream is still a stall, not something to reconnect over.
#[tokio::test]
async fn a_provider_error_still_reaches_the_viewer() {
    let options = live_options(false, PlaylistItemType::Live);
    let terminal = stream::iter(vec![Err(StreamError::ProviderIdleTimeout)]).boxed();

    let stream = enforce_live_mpegts_body_continuity(terminal, options, || async { None });

    let collected: Vec<Result<Bytes, StreamError>> = stream.collect().await;

    assert!(
        collected.iter().any(|item| matches!(item, Err(StreamError::ProviderIdleTimeout))),
        "a terminal provider error must not be swallowed by the reconnect"
    );
}

/// The reconnect is rate limited, so an instantly closing provider cannot become a request flood.
#[tokio::test]
async fn reconnects_are_rate_limited() {
    let options = live_options(false, PlaylistItemType::Live);
    let opens = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&opens);
    let mut reopen = move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Some(body_of(vec![]))
        }
    };

    let started = TokioInstant::now();
    let plan = next_mpegts_episode(&options, &mut reopen, 0, started).await;
    let elapsed = started.elapsed();

    assert!(matches!(plan, EpisodePlan::Reopen { .. }), "an empty first episode still tries the next request");
    assert!(
        elapsed >= LIVE_MPEGTS_RECONNECT_MIN_INTERVAL,
        "the retry must wait at least {LIVE_MPEGTS_RECONNECT_MIN_INTERVAL:?}, waited {elapsed:?}"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 1);
}