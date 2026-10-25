use super::*;

#[test]
fn sequential_group_round_trips_yaml_and_json() {
    let yaml = "name: grouped\ntype: m3u\nurl: http://example.com/list.m3u\nsequential_group: 7\n";
    let dto: ConfigInputDto = serde_saphyr::from_str(yaml).expect("sequential group should deserialize from yaml");
    assert_eq!(dto.sequential_group, Some(7));

    let encoded_yaml = serde_saphyr::to_string(&dto).expect("sequential group should serialize to yaml");
    assert!(encoded_yaml.contains("sequential_group: 7"));

    let encoded_json = serde_json::to_string(&dto).expect("sequential group should serialize to json");
    let decoded_json: ConfigInputDto =
        serde_json::from_str(&encoded_json).expect("sequential group should deserialize from json");
    assert_eq!(decoded_json.sequential_group, Some(7));
}

#[test]
fn disable_hls_streaming_rejects_non_xtream_input() -> Result<(), String> {
    let mut input = ConfigInputDto {
        name: "hls-only".intern(),
        input_type: InputType::M3u,
        url: "http://provider.example/list.m3u".to_string(),
        options: Some(ConfigInputOptionsDto { disable_hls_streaming: true, ..ConfigInputOptionsDto::default() }),
        ..ConfigInputDto::default()
    };

    let Err(error) = prepare_dto(&mut input) else {
        return Err("non-Xtream input accepted disable_hls_streaming".to_string());
    };
    let message = error.to_string();

    assert!(message.contains("disable_hls_streaming"), "Error: {message}");
    assert!(message.contains("Xtream"), "Error: {message}");
    Ok(())
}

#[test]
fn disable_hls_streaming_allows_extensionless_live_streams() -> Result<(), String> {
    let mut input = ConfigInputDto {
        name: "xtream".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        options: Some(ConfigInputOptionsDto {
            disable_hls_streaming: true,
            xtream_live_stream_without_extension: true,
            ..ConfigInputOptionsDto::default()
        }),
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut input).map(|_| ()).map_err(|error| error.to_string())
}

#[test]
fn test_config_input_options_dto_filter_prepare_parses_valid_filter() {
    let mut dto = ConfigInputOptionsDto {
        resolve_filter: Some(r#"name ~ "test""#.to_string()),
        ..ConfigInputOptionsDto::default()
    };
    dto.prepare(None).expect("valid filter should parse");
    assert!(dto.t_resolve_filter.is_some());
}

#[test]
fn test_config_input_options_dto_filter_prepare_rejects_invalid_filter() {
    let mut dto = ConfigInputOptionsDto {
        resolve_filter: Some(r#"name ~ "["#.to_string()), // invalid regex
        ..ConfigInputOptionsDto::default()
    };
    let result = dto.prepare(None);
    assert!(result.is_err());
}

#[test]
fn test_config_input_options_dto_filter_prepare_with_unknown_template_placeholder() {
    let mut dto = ConfigInputOptionsDto {
        resolve_filter: Some(r#"name ~ "!UNKNOWN!""#.to_string()),
        ..ConfigInputOptionsDto::default()
    };
    let result = dto.prepare(None);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unknown template placeholder"));
}

#[test]
fn test_config_input_options_dto_filter_none_prepares_successfully() {
    let mut dto = ConfigInputOptionsDto { resolve_filter: None, ..ConfigInputOptionsDto::default() };
    dto.prepare(None).expect("None filter should prepare successfully");
    assert!(dto.t_resolve_filter.is_none());
}

#[test]
fn config_input_options_serializes_shared_skip_names() {
    let dto = ConfigInputOptionsDto {
        skip_live: true,
        skip_vod: true,
        skip_series: false,
        ..ConfigInputOptionsDto::default()
    };

    let value: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&dto).expect("config input options should serialize"))
            .expect("serialized json should parse");

    assert_eq!(value.get("skip_live"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(value.get("skip_vod"), Some(&serde_json::Value::Bool(true)));
    assert!(value.get("xtream_skip_live").is_none());
    assert!(value.get("stalker_skip_vod").is_none());
}

#[test]
fn config_input_options_without_update_quality_remains_backward_compatible() -> Result<(), serde_json::Error> {
    let options: ConfigInputOptionsDto = serde_json::from_str("{}")?;
    let value = serde_json::to_value(&options)?;

    assert_eq!(options.update_quality, ConfigInputUpdateQualityDto::default());
    assert!(options.is_empty());
    assert!(value.get("update_quality").is_none());
    Ok(())
}

#[test]
fn config_input_options_update_quality_round_trips_and_keeps_options_non_empty() -> Result<(), serde_json::Error> {
    let options: ConfigInputOptionsDto = serde_json::from_value(serde_json::json!({
        "update_quality": {
            "live": 95,
            "vod": 90,
            "series": 100
        }
    }))?;

    assert!(!options.is_empty());
    assert_eq!(options.update_quality.live, 95);
    assert_eq!(options.update_quality.vod, 90);
    assert_eq!(options.update_quality.series, 100);
    assert_eq!(
        serde_json::to_value(options)?,
        serde_json::json!({
            "update_quality": {
                "live": 95,
                "vod": 90,
                "series": 100
            }
        })
    );
    Ok(())
}

#[test]
fn config_input_options_prepare_validates_update_quality() {
    let mut options: ConfigInputOptionsDto = serde_saphyr::from_str("update_quality:\n  live: 101\n")
        .expect("manually edited update quality should deserialize before validation");

    let error = options.prepare(None).expect_err("invalid update quality must be rejected");

    assert!(error.to_string().contains("options.update_quality.live"));
}

#[test]
fn config_input_options_clean_resets_update_quality() {
    let mut options = ConfigInputOptionsDto {
        update_quality: ConfigInputUpdateQualityDto { live: 95, vod: 90, series: 85 },
        ..ConfigInputOptionsDto::default()
    };

    options.clean();

    assert_eq!(options.update_quality, ConfigInputUpdateQualityDto::default());
    assert!(options.is_empty());
}

#[test]
fn flussonic_archive_window_config_validates_and_cleans() -> Result<(), Box<dyn std::error::Error>> {
    use crate::model::Prepare;
    let mut options: ConfigInputOptionsDto = serde_json::from_str(
        r#"{"flussonic_hls_catchup":"bounded_archive","flussonic_hls_catchup_max_duration_secs":1800}"#,
    )?;
    options.prepare(None)?;
    assert_eq!(options.flussonic_hls_catchup_max_duration_secs, 1800);
    assert_eq!(serde_json::from_str::<ConfigInputOptionsDto>(&serde_json::to_string(&options)?)?, options);
    for value in [0, 604801, u32::MAX] {
        options.flussonic_hls_catchup_max_duration_secs = value;
        assert!(options.prepare(None).is_err());
    }
    options.clean();
    assert_eq!(options.flussonic_hls_catchup_max_duration_secs, 14400);
    assert!(options.is_empty());
    assert!(serde_json::to_value(options)?.get("flussonic_hls_catchup_max_duration_secs").is_none());
    Ok(())
}

#[test]
fn flussonic_hls_catchup_round_trips_and_cleans() -> Result<(), serde_json::Error> {
    let defaults: ConfigInputOptionsDto = serde_json::from_str("{}")?;
    assert_eq!(defaults.flussonic_hls_catchup, super::super::FlussonicHlsCatchup::Native);
    assert!(serde_json::to_value(defaults)?.get("flussonic_hls_catchup").is_none());
    let mut options: ConfigInputOptionsDto = serde_json::from_str(r#"{"flussonic_hls_catchup":"bounded_archive"}"#)?;
    assert_eq!(options.flussonic_hls_catchup, super::super::FlussonicHlsCatchup::BoundedArchive);
    assert!(!options.is_empty());
    assert_eq!(serde_json::to_value(&options)?, serde_json::json!({"flussonic_hls_catchup": "bounded_archive"}));
    assert_eq!(serde_json::from_str::<ConfigInputOptionsDto>(&serde_json::to_string(&options)?)?, options);
    assert!(serde_json::from_str::<ConfigInputOptionsDto>(r#"{"flussonic_hls_catchup":"unknown"}"#).is_err());
    options.clean();
    assert_eq!(options.flussonic_hls_catchup, super::super::FlussonicHlsCatchup::Native);
    assert!(options.is_empty());
    Ok(())
}

#[test]
fn disable_hls_streaming_defaults_to_false() -> Result<(), serde_json::Error> {
    let options: ConfigInputOptionsDto = serde_json::from_str("{}")?;
    assert!(!options.disable_hls_streaming);
    Ok(())
}

#[test]
fn disable_hls_streaming_keeps_options_non_empty_and_clean_resets_it() {
    let mut options = ConfigInputOptionsDto { disable_hls_streaming: true, ..ConfigInputOptionsDto::default() };

    assert!(!options.is_empty());
    options.clean();
    assert!(!options.disable_hls_streaming);
    assert!(options.is_empty());
}

#[test]
fn disable_hls_streaming_round_trips() -> Result<(), serde_json::Error> {
    let options = ConfigInputOptionsDto { disable_hls_streaming: true, ..ConfigInputOptionsDto::default() };
    let json = serde_json::to_string(&options)?;
    let restored: ConfigInputOptionsDto = serde_json::from_str(&json)?;

    assert!(restored.disable_hls_streaming);
    Ok(())
}

#[test]
fn proxy_stream_fallback_defaults_off_and_is_omitted_when_disabled() -> Result<(), serde_json::Error> {
    let options: ConfigInputOptionsDto = serde_json::from_str("{}")?;
    assert!(!options.proxy_stream_fallback, "streaming must stay direct unless the input opts in");

    let json = serde_json::to_string(&ConfigInputOptionsDto::default())?;
    assert!(!json.contains("proxy_stream_fallback"), "a disabled fallback must not be persisted: {json}");
    Ok(())
}

#[test]
fn proxy_stream_fallback_keeps_options_non_empty_cleans_and_round_trips() -> Result<(), serde_json::Error> {
    let mut options = ConfigInputOptionsDto { proxy_stream_fallback: true, ..ConfigInputOptionsDto::default() };

    assert!(!options.is_empty(), "an enabled fallback must survive the editor's options pruning");
    let json = serde_json::to_string(&options)?;
    let restored: ConfigInputOptionsDto = serde_json::from_str(&json)?;
    assert!(restored.proxy_stream_fallback);

    options.clean();
    assert!(!options.proxy_stream_fallback);
    assert!(options.is_empty());
    Ok(())
}

#[test]
fn flussonic_hls_audio_tracks_round_trips_defaults_off_and_cleans() -> Result<(), serde_json::Error> {
    assert!(!ConfigInputOptionsDto::default().flussonic_hls_audio_tracks);
    let mut options = ConfigInputOptionsDto { flussonic_hls_audio_tracks: true, ..ConfigInputOptionsDto::default() };
    let json = serde_json::to_string(&options)?;
    let restored: ConfigInputOptionsDto = serde_json::from_str(&json)?;

    assert!(restored.flussonic_hls_audio_tracks);
    assert!(!options.is_empty());
    options.clean();
    assert!(!options.flussonic_hls_audio_tracks);
    assert!(options.is_empty());
    Ok(())
}

#[test]
fn user_agent_stream_index_round_trips_and_keeps_options_non_empty() -> Result<(), serde_json::Error> {
    let mut options = ConfigInputOptionsDto { user_agent_stream_index: true, ..ConfigInputOptionsDto::default() };
    let json = serde_json::to_string(&options)?;
    let restored: ConfigInputOptionsDto = serde_json::from_str(&json)?;

    assert!(restored.user_agent_stream_index);
    assert!(!options.is_empty());
    options.clean();
    assert!(!options.user_agent_stream_index);
    assert!(options.is_empty());
    Ok(())
}

#[test]
fn xtream_input_proxy_is_normalized_and_serialized() {
    let mut dto = ConfigInputDto {
        name: "proxied".intern(),
        input_type: InputType::Xtream,
        url: "https://provider.example".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        proxy: Some(ProxyConfigDto {
            url: " socks5h://proxy.example:1080 ".to_string(),
            username: Some(" proxy-user ".to_string()),
            password: Some(" proxy-pass ".to_string()),
        }),
        ..ConfigInputDto::default()
    };

    prepare_dto(&mut dto).expect("Xtream proxy should prepare");
    let proxy = dto.proxy.as_ref().expect("proxy should remain configured");
    assert_eq!(proxy.url, "socks5h://proxy.example:1080");
    assert_eq!(proxy.username.as_deref(), Some("proxy-user"));
    assert_eq!(proxy.password.as_deref(), Some("proxy-pass"));
    let yaml = serde_saphyr::to_string(&dto).expect("input should serialize");
    assert!(yaml.contains("proxy:"));
    assert!(yaml.contains("socks5h://proxy.example:1080"));
}

#[test]
fn input_proxy_is_rejected_for_non_xtream_inputs() {
    let mut dto = ConfigInputDto {
        name: "m3u".intern(),
        input_type: InputType::M3u,
        url: "https://provider.example/playlist.m3u".to_string(),
        proxy: Some(ProxyConfigDto { url: "http://proxy.example:8080".to_string(), ..Default::default() }),
        ..ConfigInputDto::default()
    };

    let error = prepare_dto(&mut dto).expect_err("M3U input proxy must be rejected");
    assert!(error.to_string().contains("only for Xtream inputs"));
}

#[test]
fn xtream_input_proxy_rejects_unsupported_scheme() {
    let mut dto = ConfigInputDto {
        name: "proxied".intern(),
        input_type: InputType::Xtream,
        url: "https://provider.example".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        proxy: Some(ProxyConfigDto { url: "ftp://proxy.example".to_string(), ..Default::default() }),
        ..ConfigInputDto::default()
    };

    let error = prepare_dto(&mut dto).expect_err("unsupported proxy scheme must be rejected");
    assert!(error.to_string().contains("Unsupported proxy scheme"));
}

#[test]
fn connection_limit_policy_defaults_to_strict_and_is_omitted_on_serialize() -> Result<(), serde_json::Error> {
    let options: ConfigInputOptionsDto = serde_json::from_str("{}")?;
    assert_eq!(options.connection_limit_policy, ConnectionLimitPolicy::Strict);
    assert!(options.is_empty());

    let json = serde_json::to_value(&options)?;
    assert!(json.get("connection_limit_policy").is_none());
    Ok(())
}

#[test]
fn connection_limit_policy_round_trips_non_default() -> Result<(), serde_json::Error> {
    let options = ConfigInputOptionsDto {
        connection_limit_policy: ConnectionLimitPolicy::RotateOnFull,
        ..ConfigInputOptionsDto::default()
    };
    assert!(!options.is_empty());
    let json = serde_json::to_string(&options)?;
    assert!(json.contains("rotate_on_full"));
    let restored: ConfigInputOptionsDto = serde_json::from_str(&json)?;
    assert_eq!(restored.connection_limit_policy, ConnectionLimitPolicy::RotateOnFull);
    Ok(())
}
