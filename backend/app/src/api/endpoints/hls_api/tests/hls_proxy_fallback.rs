use super::*;
use crate::api::endpoints::hls_api::segment::download_legacy_hls_manifest_with_egress;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MEDIA_PLAYLIST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nsegment.ts\n";

#[tokio::test]
async fn legacy_manifest_uses_direct_then_authenticated_proxy_for_both_redirect_hops() {
    let origin = TcpListener::bind("127.0.0.1:0").await.expect("reserve unavailable origin port");
    let origin_addr = origin.local_addr().expect("origin addr");
    drop(origin);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = listener.local_addr().expect("proxy addr");
    let proxy = tokio::spawn(async move {
        let mut requests = Vec::new();
        for hop in 0..2 {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(12), listener.accept())
                .await
                .expect("proxied hop arrived")
                .expect("proxy accept");
            let mut head = Vec::new();
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut buf = [0; 2048];
                let n = socket.read(&mut buf).await.expect("read proxy request");
                assert!(n > 0 && head.len() < 16384, "complete proxy request head");
                head.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            assert!(head.to_ascii_lowercase().contains("proxy-authorization: basic "), "hop {hop} must authenticate");
            requests.push(head.lines().next().unwrap_or_default().to_string());
            let reply = if hop == 0 {
                "HTTP/1.1 302 Found\r\nLocation: http://cdn.example.invalid/media.m3u8\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{MEDIA_PLAYLIST}", MEDIA_PLAYLIST.len())
            };
            socket.write_all(reply.as_bytes()).await.expect("write proxy response");
        }
        requests
    });

    let app = test_app_state();
    app.http_clients.stream.store(Arc::new(tuliprox_core::utils::request::create_stream_client(
        &app.app_config,
        reqwest::redirect::Policy::limited(10),
    ).expect("direct client")));
    let mut input = crate::model::InputSource {
        name: Arc::from("hls-proxy-test"),
        url: format!("http://{origin_addr}/entry.m3u8"),
        provider: None,
        username: None,
        password: None,
        proxy: Some(tuliprox_core::model::ProxyConfig {
            url: format!("http://{proxy_addr}"),
            username: Some("test-user".to_string()),
            password: Some("test-pass".to_string()),
        }),
        proxy_stream_fallback: true,
        proxy_stream_first: false,
        method: shared::model::InputFetchMethod::GET,
        headers: HashMap::new(),
    };
    let (body, final_url, _, used_proxy) = download_legacy_hls_manifest_with_egress(
        &app, &input, &HeaderMap::new(),
    ).await.expect("fallback manifest");
    assert_eq!(body, MEDIA_PLAYLIST);
    assert_eq!(final_url, "http://cdn.example.invalid/media.m3u8");
    assert!(used_proxy);
    let requests = proxy.await.expect("proxy task");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("/entry.m3u8"));
    assert!(requests[1].contains("cdn.example.invalid/media.m3u8"));

    input.proxy_stream_fallback = false;
    let err = download_legacy_hls_manifest_with_egress(&app, &input, &HeaderMap::new()).await;
    assert!(err.is_err(), "disabled input must never use the proxy");
}

#[tokio::test]
async fn legacy_manifest_direct_success_does_not_use_proxy() {
    let origin = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let origin_addr = origin.local_addr().expect("origin addr");
    let direct = tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.expect("origin accept");
        let mut head = [0; 2048];
        let _ = socket.read(&mut head).await.expect("read request");
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{MEDIA_PLAYLIST}",
            MEDIA_PLAYLIST.len()
        );
        socket.write_all(reply.as_bytes()).await.expect("answer manifest");
    });
    let proxy = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = proxy.local_addr().expect("proxy addr");
    let app = test_app_state();
    app.http_clients.stream.store(Arc::new(tuliprox_core::utils::request::create_stream_client(
        &app.app_config,
        reqwest::redirect::Policy::limited(10),
    ).expect("direct client")));
    let input = crate::model::InputSource {
        name: Arc::from("hls-direct-test"),
        url: format!("http://{origin_addr}/live.m3u8"),
        provider: None,
        username: None,
        password: None,
        proxy: Some(tuliprox_core::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        proxy_stream_fallback: true,
        proxy_stream_first: false,
        method: shared::model::InputFetchMethod::GET,
        headers: HashMap::new(),
    };
    let (body, _, _, used_proxy) = download_legacy_hls_manifest_with_egress(&app, &input, &HeaderMap::new())
        .await
        .expect("direct manifest succeeds");
    assert_eq!(body, MEDIA_PLAYLIST);
    assert!(!used_proxy, "success on the direct path must not select the proxy for child media");
    direct.await.expect("origin task");
    assert!(tokio::time::timeout(Duration::from_millis(250), proxy.accept()).await.is_err(), "proxy remains untouched");
}

#[tokio::test]
async fn legacy_manifest_404_falls_back_to_the_proxy() {
    // The panel answers a firewalled client with 404; the proxy egress must get one attempt
    // before the 404 is declared final.
    let origin = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let origin_addr = origin.local_addr().expect("origin addr");
    let direct = tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.expect("origin accept");
        let mut head = [0; 2048];
        let _ = socket.read(&mut head).await.expect("read request");
        socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.expect("answer 404");
    });
    let proxy = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = proxy.local_addr().expect("proxy addr");
    let proxied = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), proxy.accept())
            .await
            .expect("the 404 reaches the proxy")
            .expect("proxy accept");
        let mut head = [0; 2048];
        let _ = socket.read(&mut head).await.expect("read proxy request");
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{MEDIA_PLAYLIST}",
            MEDIA_PLAYLIST.len()
        );
        socket.write_all(reply.as_bytes()).await.expect("answer manifest through the proxy");
    });
    let app = test_app_state();
    app.http_clients.stream.store(Arc::new(tuliprox_core::utils::request::create_stream_client(
        &app.app_config,
        reqwest::redirect::Policy::limited(10),
    ).expect("direct client")));
    let input = crate::model::InputSource {
        name: Arc::from("hls-404-test"),
        url: format!("http://{origin_addr}/missing.m3u8"),
        provider: None,
        username: None,
        password: None,
        proxy: Some(tuliprox_core::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        proxy_stream_fallback: true,
        proxy_stream_first: false,
        method: shared::model::InputFetchMethod::GET,
        headers: HashMap::new(),
    };
    let (body, _, _, used_proxy) = download_legacy_hls_manifest_with_egress(&app, &input, &HeaderMap::new())
        .await
        .expect("the proxy egress recovers the firewalled 404");
    assert_eq!(body, MEDIA_PLAYLIST);
    assert!(used_proxy, "the successful retry came from the proxy");
    direct.await.expect("origin task");
    proxied.await.expect("proxy task");
}

#[tokio::test]
async fn proxy_first_manifest_opens_through_the_proxy_without_touching_direct() {
    let origin = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let origin_addr = origin.local_addr().expect("origin addr");
    let proxy = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let proxy_addr = proxy.local_addr().expect("proxy addr");
    let proxied = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), proxy.accept())
            .await
            .expect("the proxy receives the manifest")
            .expect("proxy accept");
        let mut head = [0; 2048];
        let _ = socket.read(&mut head).await.expect("read proxy request");
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{MEDIA_PLAYLIST}",
            MEDIA_PLAYLIST.len()
        );
        socket.write_all(reply.as_bytes()).await.expect("answer manifest through the proxy");
    });
    let app = test_app_state();
    app.http_clients.stream.store(Arc::new(tuliprox_core::utils::request::create_stream_client(
        &app.app_config,
        reqwest::redirect::Policy::limited(10),
    ).expect("direct client")));
    let input = crate::model::InputSource {
        name: Arc::from("hls-proxy-first-test"),
        url: format!("http://{origin_addr}/live.m3u8"),
        provider: None,
        username: None,
        password: None,
        proxy: Some(tuliprox_core::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        proxy_stream_fallback: false,
        proxy_stream_first: true,
        method: shared::model::InputFetchMethod::GET,
        headers: HashMap::new(),
    };
    let (body, _, _, used_proxy) = download_legacy_hls_manifest_with_egress(&app, &input, &HeaderMap::new())
        .await
        .expect("proxy-first manifest succeeds");
    assert_eq!(body, MEDIA_PLAYLIST);
    assert!(used_proxy, "the proxy is the first egress");
    proxied.await.expect("proxy task");
    assert!(
        tokio::time::timeout(Duration::from_millis(250), origin.accept()).await.is_err(),
        "proxy-first never touches the direct origin on success"
    );
}

#[test]
fn hls_proxy_fallback_rejects_local_body_and_redirect_failures() {
    use std::io::{Error, ErrorKind};
    let allowed = [
        Error::new(ErrorKind::TimedOut, "timed out"),
        Error::other("Request error: connection failed"),
        Error::other("All attempts and providers exhausted"),
        Error::other("Request failed (404 Not Found): origin"),
        Error::other("Request failed (502 Bad Gateway): origin"),
    ];
    for error in allowed {
        assert!(super::super::egress::hls_proxy_fallback_candidate(&error), "eligible: {error}");
    }
    let disallowed = [
        Error::other("Request failed (503 Service Unavailable): origin"),
        Error::other("Request failed (401 Unauthorized): origin"),
        Error::other("decoded body limit exceeded"),
        Error::other("invalid redirect Location"),
        Error::new(ErrorKind::InvalidData, "invalid utf-8"),
    ];
    for error in disallowed {
        assert!(!super::super::egress::hls_proxy_fallback_candidate(&error), "not an egress failure: {error}");
    }
}
