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
        ProviderStreamRequestFailure::HlsStatus { status: StatusCode::BAD_GATEWAY, headers: Vec::new() },
    ];
    for failure in transport {
        assert!(
            failure.is_proxy_fallback_candidate(),
            "{} / {} must be retried through the proxy",
            failure.status(),
            failure.provider_error_class()
        );
    }

    // A 404 must be retried on the other egress: this panel answers a firewalled client with 404.
    let firewall_block_404 = [
        ProviderStreamRequestFailure::ChannelMissing {
            status: StatusCode::NOT_FOUND,
            provider_error_class: "http_4xx",
        },
        ProviderStreamRequestFailure::HlsStatus { status: StatusCode::NOT_FOUND, headers: Vec::new() },
    ];
    for failure in firewall_block_404 {
        assert!(
            failure.is_proxy_fallback_candidate(),
            "a 404 may be a firewall block and must reach the other egress",
        );
    }

    let provider_answers = [
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
        ProviderStreamRequestFailure::HlsStatus { status: StatusCode::UNAUTHORIZED, headers: Vec::new() },
        ProviderStreamRequestFailure::HlsStatus { status: StatusCode::SERVICE_UNAVAILABLE, headers: Vec::new() },
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

#[tokio::test]
async fn hls_segment_uses_manifest_proxy_egress_without_retrying_direct() {
    let origin = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let origin_addr = origin.local_addr().expect("origin addr");
    let proxy = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = proxy.local_addr().expect("proxy addr");
    let requests = tokio::spawn(async move {
        let mut requests = Vec::new();
        for hop in 0..2 {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), proxy.accept())
                .await
                .expect("proxy sees both hops")
                .expect("proxy accepts request");
            let request = read_http_request(&mut socket).await;
            assert!(request.to_ascii_lowercase().contains("proxy-authorization: basic "), "hop {hop} authenticates");
            requests.push(request.lines().next().unwrap_or_default().to_string());
            let response = if hop == 0 {
                b"HTTP/1.1 302 Found\r\nLocation: http://cdn.example.invalid/hls/segment.ts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 4\r\nConnection: close\r\n\r\nmedia".to_vec()
            };
            socket.write_all(&response).await.expect("proxy answers");
        }
        requests
    });
    let url = Url::parse(&format!("http://{origin_addr}/hls/segment.ts")).expect("origin URL");
    let headers = HeaderMap::new();
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: false,
        buffer_size: 0,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::HlsResource,
        hls_manifest_used_proxy: true,
    };
    let mut options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr: "127.0.0.1:8080".parse().expect("addr"),
        item_type: PlaylistItemType::LiveHls,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &url,
        req_headers: &headers,
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::Identity,
    });
    options.flags.set(ProviderStreamFactoryFlags::ProxyStreamFallback);
    options.stream_proxy = Some(tuliprox_core::model::ProxyConfig {
        url: format!("http://{proxy_addr}"),
        username: Some("test-user".to_string()),
        password: Some("test-pass".to_string()),
    });
    let app = proxy_configured_app_config();
    let event_manager = Arc::new(crate::api::model::EventManager::new());
    let active_provider = Arc::new(crate::api::model::ActiveProviderManager::new(&app, &event_manager));
    let geoip = Arc::new(ArcSwapOption::<crate::repository::GeoIp>::default());
    let config = app.config.load();
    let active_users = Arc::new(crate::api::model::ActiveUserManager::new(&config, &geoip, &event_manager));
    drop(config);
    let shared_stream_manager = Arc::new(crate::api::model::SharedStreamManager::new(Arc::clone(&active_provider)));
    let connection_manager = Arc::new(crate::api::model::ConnectionManager::new(
        &active_users,
        &active_provider,
        &shared_stream_manager,
        &event_manager,
        None,
    ));
    let ctx = tuliprox_session::stream_ctx::ProviderStreamCtx {
        active_provider,
        app_config: app,
        connection_manager,
        http_clients: Arc::new(tuliprox_core::model::HttpClients::default()),
    };
    let response = super::request::get_provider_stream(&ctx, &reqwest::Client::new(), &options)
        .await
        .expect("proxied segment opens")
        .expect("segment response");
    assert_eq!(response.info.as_ref().map(|(_, status, _, _)| *status), Some(StatusCode::OK));
    let requests = requests.await.expect("proxy task");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("/hls/segment.ts"));
    assert!(requests[1].contains("cdn.example.invalid/hls/segment.ts"));
    assert!(tokio::time::timeout(Duration::from_millis(250), origin.accept()).await.is_err(), "direct origin is untouched");
}

#[tokio::test]
async fn hls_segment_404_retries_the_proxy_before_staying_an_error() {
    let origin = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let origin_addr = origin.local_addr().expect("origin addr");
    let direct = tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.expect("accept origin");
        let _ = read_http_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("answer 404");
    });
    let proxy = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = proxy.local_addr().expect("proxy addr");
    let proxied = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), proxy.accept())
            .await
            .expect("the 404 reaches the proxy")
            .expect("proxy accepts");
        let _ = read_http_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("proxy repeats the 404");
    });
    let url = Url::parse(&format!("http://{origin_addr}/hls/segment.ts")).expect("URL");
    let headers = HeaderMap::new();
    let stream_options = StreamOptions {
        stream_retry: false,
        buffer_enabled: false,
        buffer_size: 0,
        buffer_max_bytes: 0,
        pipe_provider_stream: true,
        response_mode: StreamResponseMode::HlsResource,
        hls_manifest_used_proxy: false,
    };
    let mut options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr: "127.0.0.1:8080".parse().expect("addr"),
        item_type: PlaylistItemType::LiveHls,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &url,
        req_headers: &headers,
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::Identity,
    });
    options.flags.set(ProviderStreamFactoryFlags::ProxyStreamFallback);
    options.stream_proxy = Some(tuliprox_core::model::ProxyConfig {
        url: format!("http://{proxy_addr}"),
        username: None,
        password: None,
    });
    let app = proxy_configured_app_config();
    let event_manager = Arc::new(crate::api::model::EventManager::new());
    let active_provider = Arc::new(crate::api::model::ActiveProviderManager::new(&app, &event_manager));
    let geoip = Arc::new(ArcSwapOption::<crate::repository::GeoIp>::default());
    let config = app.config.load();
    let active_users = Arc::new(crate::api::model::ActiveUserManager::new(&config, &geoip, &event_manager));
    drop(config);
    let shared_stream_manager = Arc::new(crate::api::model::SharedStreamManager::new(Arc::clone(&active_provider)));
    let connection_manager = Arc::new(crate::api::model::ConnectionManager::new(
        &active_users,
        &active_provider,
        &shared_stream_manager,
        &event_manager,
        None,
    ));
    let ctx = tuliprox_session::stream_ctx::ProviderStreamCtx {
        active_provider,
        app_config: app,
        connection_manager,
        http_clients: Arc::new(tuliprox_core::model::HttpClients::default()),
    };
    let failure = match super::request::get_provider_stream(&ctx, &reqwest::Client::new(), &options).await {
        Err(failure) => failure,
        Ok(_) => panic!("a 404 on both egresses must remain an error"),
    };
    assert!(matches!(failure, ProviderStreamRequestFailure::HlsStatus { status: StatusCode::NOT_FOUND, .. }));
    direct.await.expect("origin task");
    proxied.await.expect("proxy task");
}

#[tokio::test]
async fn proxy_first_stream_opens_through_the_proxy_without_touching_direct() {
    let origin = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let origin_addr = origin.local_addr().expect("origin addr");
    let proxy = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = proxy.local_addr().expect("proxy addr");
    let proxied = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), proxy.accept())
            .await
            .expect("the proxy receives the stream open")
            .expect("proxy accepts");
        let _ = read_http_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 4\r\nConnection: close\r\n\r\nmedia")
            .await
            .expect("proxy answers media");
    });
    let url = Url::parse(&format!("http://{origin_addr}/live/1.ts")).expect("URL");
    let headers = HeaderMap::new();
    let stream_options = StreamOptions {
        stream_retry: false,
        buffer_enabled: false,
        buffer_size: 0,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::HlsResource,
        hls_manifest_used_proxy: false,
    };
    let mut options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr: "127.0.0.1:8080".parse().expect("addr"),
        item_type: PlaylistItemType::LiveHls,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &url,
        req_headers: &headers,
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::Identity,
    });
    options.flags.set(ProviderStreamFactoryFlags::ProxyStreamFirst);
    options.stream_proxy = Some(tuliprox_core::model::ProxyConfig {
        url: format!("http://{proxy_addr}"),
        username: None,
        password: None,
    });
    let app = proxy_configured_app_config();
    let event_manager = Arc::new(crate::api::model::EventManager::new());
    let active_provider = Arc::new(crate::api::model::ActiveProviderManager::new(&app, &event_manager));
    let geoip = Arc::new(ArcSwapOption::<crate::repository::GeoIp>::default());
    let config = app.config.load();
    let active_users = Arc::new(crate::api::model::ActiveUserManager::new(&config, &geoip, &event_manager));
    drop(config);
    let shared_stream_manager = Arc::new(crate::api::model::SharedStreamManager::new(Arc::clone(&active_provider)));
    let connection_manager = Arc::new(crate::api::model::ConnectionManager::new(
        &active_users,
        &active_provider,
        &shared_stream_manager,
        &event_manager,
        None,
    ));
    let ctx = tuliprox_session::stream_ctx::ProviderStreamCtx {
        active_provider,
        app_config: app,
        connection_manager,
        http_clients: Arc::new(tuliprox_core::model::HttpClients::default()),
    };
    let response = super::request::get_provider_stream(&ctx, &reqwest::Client::new(), &options)
        .await
        .expect("proxy-first stream opens")
        .expect("stream response");
    assert_eq!(response.info.as_ref().map(|(_, status, _, _)| *status), Some(StatusCode::OK));
    proxied.await.expect("proxy task");
    assert!(
        tokio::time::timeout(Duration::from_millis(250), origin.accept()).await.is_err(),
        "proxy-first never touches the direct origin"
    );
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