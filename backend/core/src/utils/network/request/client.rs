use super::{get_request_headers, log_proxy_diagnostics};
use crate::model::{AppConfig, Config, ProxyConfig, ReverseProxyDisabledHeaderConfig};
use log::{error, info};
use reqwest::redirect::Policy;
use shared::{error::TuliproxError, model::InputFetchMethod};
use std::{collections::HashMap, time::Duration};
use url::Url;

pub fn get_client_request<S: ::std::hash::BuildHasher + Default>(
    client: &reqwest::Client,
    method: InputFetchMethod,
    headers: Option<&HashMap<String, String, S>>,
    url: &Url,
    custom_headers: Option<&HashMap<String, Vec<u8>, S>>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = match method {
        InputFetchMethod::GET => client.get(url.clone()),
        InputFetchMethod::POST => {
            // let base_url = url[..url::Position::BeforePath].to_string() + url.path();
            let mut params: HashMap<String, String, S> = HashMap::default();
            for (key, value) in url.query_pairs() {
                params.insert(key.to_string(), value.to_string());
            }
            // we could cut the params but we leave them as query and add them as form.
            client.post(url.clone()).form(&params)
        }
    };
    let headers = get_request_headers(headers, custom_headers, disabled_headers, default_user_agent);
    request.headers(headers)
}

pub fn create_client_with_redirect(cfg: &AppConfig, redirect_policy: Policy) -> reqwest::ClientBuilder {
    let config = cfg.config.load();
    match configured_client_with_proxy(&config, redirect_policy, config.proxy.as_ref()) {
        Ok(builder) => builder,
        Err(err) => {
            // The global proxy is validated at config load; a failure here is a startup bug.
            error!("Failed to apply prepared global proxy configuration: {err}");
            reqwest::Client::builder()
        }
    }
}

/// Applies one prepared proxy configuration to a client builder.
///
/// Fails closed: an unusable proxy must not silently degrade into a direct connection.
fn apply_proxy_config(
    client: reqwest::ClientBuilder,
    proxy_cfg: &ProxyConfig,
) -> Result<reqwest::ClientBuilder, TuliproxError> {
    let mut url = Url::parse(&proxy_cfg.url)
        .map_err(|err| TuliproxError::ConfigProxy(format!("Invalid proxy URL: {err}")))?;
    let scheme = url.scheme().to_ascii_lowercase();

    let proxy = match scheme.as_str() {
        "socks5" | "socks5h" => {
            if let Some(user) = &proxy_cfg.username {
                let _ = url.set_username(user);
            }
            if let Some(pass) = &proxy_cfg.password {
                let _ = url.set_password(Some(pass));
            }
            reqwest::Proxy::all(url.as_str())
        }
        "http" | "https" => reqwest::Proxy::all(url.as_str()).map(|proxy| {
            if let (Some(username), Some(password)) = (&proxy_cfg.username, &proxy_cfg.password) {
                proxy.basic_auth(username, password)
            } else {
                proxy
            }
        }),
        _ => return Err(TuliproxError::ConfigProxy(format!("Unsupported proxy scheme: {scheme}"))),
    }
    .map_err(|err| TuliproxError::ConfigProxy(format!("Failed to create proxy: {err}")))?;

    Ok(client.proxy(proxy))
}

fn configured_client_with_proxy(
    config: &Config,
    redirect_policy: Policy,
    proxy: Option<&ProxyConfig>,
) -> Result<reqwest::ClientBuilder, TuliproxError> {
    log_proxy_diagnostics(config);
    let mut client = reqwest::Client::builder()
        .redirect(redirect_policy)
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .danger_accept_invalid_certs(config.accept_insecure_ssl_certificates);

    if let Some(proxy_cfg) = proxy {
        client = apply_proxy_config(client, proxy_cfg)?;
    }

    if let Some(rp_config) = config.reverse_proxy.as_ref() {
        if rp_config.disabled_header.as_ref().is_some_and(|d| d.referer_header) {
            client = client.referer(false);
        }
    }

    Ok(client)
}

pub fn create_client(cfg: &AppConfig) -> reqwest::ClientBuilder {
    create_client_with_redirect(cfg, Policy::limited(10))
}

/// Dedicated discovery profile. Callers must propagate construction failure, never
/// fall back to an unconfigured client. Builder access permits local CA/DNS fixtures.
pub fn create_tmdb_client(cfg: &AppConfig) -> Result<reqwest::ClientBuilder, TuliproxError> {
    let config = cfg.config.load();
    let invalid_proxy = || TuliproxError::Config("TMDB HTTP profile has invalid proxy configuration".to_string());
    // The general factory historically logs malformed proxies and continues. TMDB
    // must fail closed, using the same snapshot and proxy transformations instead.
    if let Some(proxy) = &config.proxy {
        let mut url = Url::parse(&proxy.url).map_err(|_| invalid_proxy())?;
        match url.scheme() {
            "socks5" | "socks5h" => {
                if let Some(user) = &proxy.username {
                    url.set_username(user).map_err(|()| invalid_proxy())?;
                }
                if let Some(pass) = &proxy.password {
                    url.set_password(Some(pass)).map_err(|()| invalid_proxy())?;
                }
            }
            "http" | "https" => {}
            _ => return Err(invalid_proxy()),
        }
        reqwest::Proxy::all(url.as_str()).map_err(|_| invalid_proxy())?;
    }
    // Discovery carries its own Bearer credential: never inherit the generic TLS bypass.
    // Keep configured proxy/auth and trusted roots, including hostname verification.
    let mut builder = configured_client_with_proxy(&config, Policy::none(), config.proxy.as_ref())
        .map_err(|_| invalid_proxy())?
        .danger_accept_invalid_certs(false)
        .retry(reqwest::retry::never())
        .http1_only();
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    Ok(builder)
}

/// Builds a dedicated HTTP client for one input's own proxy.
///
/// `fallback` is returned unchanged when the input has no proxy.
pub fn create_input_http_client_with_redirect(
    cfg: &AppConfig,
    fallback: &reqwest::Client,
    proxy: Option<&ProxyConfig>,
    redirect_policy: Policy,
) -> Result<reqwest::Client, TuliproxError> {
    let Some(proxy) = proxy else { return Ok(fallback.clone()) };
    info!("Creating HTTP client with input proxy: {}", proxy.display_for_log());
    let config = cfg.config.load();
    let mut builder = configured_client_with_proxy(&config, redirect_policy, Some(proxy))?.http1_only();
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    builder
        .build()
        .map_err(|err| TuliproxError::ConfigProxy(format!("Failed to create HTTP client for input proxy: {err}")))
}

pub fn create_input_http_client(
    cfg: &AppConfig,
    fallback: &reqwest::Client,
    proxy: Option<&ProxyConfig>,
) -> Result<reqwest::Client, TuliproxError> {
    create_input_http_client_with_redirect(cfg, fallback, proxy, Policy::limited(10))
}

pub fn create_input_http_client_no_redirect(
    cfg: &AppConfig,
    fallback: &reqwest::Client,
    proxy: Option<&ProxyConfig>,
) -> Result<reqwest::Client, TuliproxError> {
    create_input_http_client_with_redirect(cfg, fallback, proxy, Policy::none())
}
