use super::*;
use crate::api::model::streams::provider_stream_factory::error::{
    classify_provider_status_error, is_account_full_status, status_from_io_error, ProviderStreamRequestFailure,
};
use std::time::Duration;

/// Playback must never be routed through a configured proxy.
///
/// A proxy adds a hop the provider does not see for a direct client and hides the client's
/// address, so a working channel can start failing for reasons that belong to the proxy. This
/// pins the guarantee at the client level: the stream clients are built with `no_proxy()`.
#[tokio::test]
async fn stream_client_never_uses_a_configured_proxy() {
    let app_config = proxy_configured_app_config();
    let client = tuliprox_core::utils::request::create_stream_client(
        &app_config,
        reqwest::redirect::Policy::limited(10),
    )
    .expect("stream client builds");

    // The only observable proof is the request path: with a reachable proxy configured, a client
    // that honoured it would send the request to the proxy instead of the origin.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let Ok(Ok((mut socket, _))) = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await else {
            return false;
        };
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while let Ok(read) = socket.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
        !String::from_utf8_lossy(&request).contains("Proxy-Connection")
    });

    let response = client.get(format!("http://{addr}/live/1.ts")).send().await;
    assert!(response.is_ok(), "the direct connection must reach the origin: {response:?}");
    assert!(server.await.expect("server task"), "the request must not be proxied");
}

/// A 404 is the panel answering "this account has no such channel", not a dead stream.
#[test]
fn not_found_is_an_account_scoped_channel_rejection() {
    let failure = ProviderStreamRequestFailure::ChannelMissing {
        status: StatusCode::NOT_FOUND,
        provider_error_class: classify_provider_status_error(StatusCode::NOT_FOUND),
    };

    assert!(failure.is_channel_missing(), "a 404 must be treated as a missing channel for this account");
    assert!(!failure.is_account_full(), "a missing channel is not a connection-limit rejection");
}

/// A 404 that the lateral failover folded into an `io::Error` is still a missing channel.
///
/// The failover engine walks every address of one account and reports the last failure as a
/// transport error. Without recovering the status, a panel that has no such channel for this login
/// looks like an unreachable origin and the request never rotates to another account.
#[test]
fn not_found_recovered_from_transport_error_is_a_missing_channel() {
    let err = std::io::Error::other("Request failed (404 Not Found): http://origin.example/live/1.ts");
    assert_eq!(status_from_io_error(&err), Some(StatusCode::NOT_FOUND));
}

#[test]
fn transport_errors_without_a_status_stay_unclassified() {
    let err = std::io::Error::other("connection reset by peer");
    assert_eq!(status_from_io_error(&err), None);
}

/// Panels answer "already at your limit" with different statuses; all must rotate.
#[test]
fn conflict_is_recognised_as_an_account_at_its_limit() {
    assert!(is_account_full_status(StatusCode::CONFLICT), "409 Conflict is this panel's account-full answer");
    assert!(is_account_full_status(StatusCode::TOO_MANY_REQUESTS));
    assert!(!is_account_full_status(StatusCode::NOT_FOUND), "a missing channel is not account-full");
}

/// Account-full keeps its own meaning, so the connection policy still governs it.
#[test]
fn account_full_stays_distinct_from_a_missing_channel() {
    let failure = ProviderStreamRequestFailure::AccountFull {
        status: StatusCode::TOO_MANY_REQUESTS,
        provider_error_class: "account_full",
    };

    assert!(failure.is_account_full());
    assert!(!failure.is_channel_missing());
}

/// Only transport-shaped failures justify a second egress.
#[test]
fn only_transport_failures_are_proxy_fallback_candidates() {
    let transport = [
        ProviderStreamRequestFailure::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            provider_error_class: "timeout",
            serve_channel_unavailable: true,
        },
        ProviderStreamRequestFailure::Status {
            status: StatusCode::BAD_GATEWAY,
            provider_error_class: "connect",
            serve_channel_unavailable: true,
        },
        ProviderStreamRequestFailure::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            provider_error_class: "http_5xx",
            serve_channel_unavailable: true,
        },
        ProviderStreamRequestFailure::Status {
            status: StatusCode::REQUEST_TIMEOUT,
            provider_error_class: "http_other",
            serve_channel_unavailable: true,
        },
    ];
    for failure in transport {
        assert!(
            failure.is_proxy_fallback_candidate(),
            "{} / {} must be retried through the proxy",
            failure.status(),
            failure.provider_error_class()
        );
    }

    let provider_answers = [
        ProviderStreamRequestFailure::ChannelMissing {
            status: StatusCode::NOT_FOUND,
            provider_error_class: "http_4xx",
        },
        ProviderStreamRequestFailure::AccountFull {
            status: StatusCode::TOO_MANY_REQUESTS,
            provider_error_class: "account_full",
        },
        ProviderStreamRequestFailure::Status {
            status: StatusCode::UNAUTHORIZED,
            provider_error_class: "http_4xx",
            serve_channel_unavailable: true,
        },
        ProviderStreamRequestFailure::Status {
            status: StatusCode::FORBIDDEN,
            provider_error_class: "http_4xx",
            serve_channel_unavailable: true,
        },
    ];
    for failure in provider_answers {
        assert!(
            !failure.is_proxy_fallback_candidate(),
            "{} / {} is the provider's answer; a proxy would only repeat it",
            failure.status(),
            failure.provider_error_class()
        );
    }
}

/// The proxy fallback must actually egress through the proxy.
///
/// The manual-redirect branch once ignored the client it was handed and used the shared
/// no-redirect one, so the fallback silently connected directly. A proxy that records every
/// request proves the egress: if the stream went around it, the recorder stays empty.
#[tokio::test]
async fn proxy_fallback_client_egresses_through_the_proxy() {
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let origin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_addr = origin_listener.local_addr().unwrap();

    // Minimal HTTP proxy: it must see the request, and it answers on the origin's behalf so the
    // test does not depend on a real origin.
    let proxy_seen = tokio::spawn(async move {
        let Ok(Ok((mut socket, _))) = tokio::time::timeout(Duration::from_secs(2), proxy_listener.accept()).await
        else {
            return false;
        };
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while let Ok(read) = socket.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
        true
    });
    // The origin must NOT be reached by the fallback.
    let origin_reached = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), origin_listener.accept()).await.is_ok()
    });

    let proxy = tuliprox_core::model::ProxyConfig {
        url: format!("http://{proxy_addr}"),
        username: None,
        password: None,
    };
    let client = tuliprox_core::utils::request::create_input_http_client_with_redirect(
        &proxy_configured_app_config(),
        &reqwest::Client::new(),
        Some(&proxy),
        reqwest::redirect::Policy::none(),
    )
    .expect("proxy client builds");

    let response = client.get(format!("http://{origin_addr}/live/1.ts")).send().await;
    assert!(response.is_ok(), "the proxy must answer the request: {response:?}");
    assert!(proxy_seen.await.expect("proxy task"), "the request must reach the proxy");
    assert!(!origin_reached.await.expect("origin task"), "the request must not bypass the proxy");
}

fn proxy_configured_app_config() -> Arc<AppConfig> {
    let mut config = Config::default();
    config.proxy = Some(tuliprox_core::model::ProxyConfig {
        url: "http://127.0.0.1:9".to_string(),
        username: None,
        password: None,
    });
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    })
}