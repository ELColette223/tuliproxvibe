use super::*;
use crate::model::config::input::migrate::normalize_lateral_url;

fn xtream_input() -> ConfigInputDto {
    ConfigInputDto {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.net".to_string(),
        ..ConfigInputDto::default()
    }
}

fn account(username: &str, password: &str) -> ConfigInputAccountDto {
    ConfigInputAccountDto {
        username: username.intern(),
        password: Some(password.to_string()),
        ..ConfigInputAccountDto::default()
    }
}

#[test]
fn accounts_round_trip_through_yaml_and_json() {
    let yaml = "name: provider\ntype: xtream\nurl_aliases:\n- http://a.net\n- http://b.net\naccounts:\n- username: one\n  password: pw1\n- username: two\n  password: pw2\n  max_connections: 4\n  url_aliases:\n  - http://a.net\n  exp_date: 1788779145\n";
    let input: ConfigInputDto = serde_saphyr::from_str(yaml).expect("parse accounts");
    assert_eq!(input.accounts.as_ref().expect("accounts").len(), 2);
    assert_eq!(input.url_aliases.as_ref().expect("laterals").len(), 2);

    let serialized = serde_saphyr::to_string(&input).expect("serialize");
    let reparsed: ConfigInputDto = serde_saphyr::from_str(&serialized).expect("reparse");
    assert_eq!(reparsed, input);

    let json = serde_json::to_string(&input).expect("json");
    assert_eq!(serde_json::from_str::<ConfigInputDto>(&json).expect("json roundtrip"), input);
}

#[test]
fn account_rejects_name_and_url_fields() {
    // The account has no `name`/`url`; a config that still uses them must fail loudly
    // instead of silently dropping the values.
    for yaml in [
        "name: provider\ntype: xtream\naccounts:\n- username: one\n  password: pw\n  name: legacy\n  url: http://a.net\n",
        "name: provider\ntype: xtream\naccounts:\n- username: one\n  password: pw\n  url: http://a.net\n",
    ] {
        assert!(serde_saphyr::from_str::<ConfigInputDto>(yaml).is_err(), "accepted: {yaml}");
    }
}

#[test]
fn prepare_assigns_ids_and_derives_the_root_from_account_one() {
    let mut input = xtream_input();
    input.url = String::new();
    input.url_aliases = Some(vec!["http://a.net".intern(), "http://b.net".intern()]);
    input.accounts = Some(vec![
        ConfigInputAccountDto { max_connections: 7, ..account("root_user", "root_pw") },
        account("second", "second_pw"),
    ]);

    let next = prepare_dto(&mut input).expect("prepare");

    let accounts = input.accounts.as_ref().expect("accounts");
    assert_eq!(accounts[0].id, 2, "ids continue from the input id");
    assert_eq!(accounts[1].id, 3);
    assert_eq!(next, 3);
    // The root fields mirror account 1, so the rest of the pipeline keeps working.
    assert_eq!(input.username.as_deref(), Some("root_user"));
    assert_eq!(input.password.as_deref(), Some("root_pw"));
    assert_eq!(input.max_connections, 7);
    assert_eq!(input.url, "http://a.net", "the first lateral becomes the root address");
    assert!(!input.account_disabled);
}

#[test]
fn duplicate_username_is_rejected() {
    let mut input = xtream_input();
    input.accounts = Some(vec![account("same", "pw1"), account("same", "pw2")]);
    let err = prepare_dto(&mut input).expect_err("duplicate username must fail");
    assert!(err.to_string().contains("duplicate account username"), "{err}");
}

#[test]
fn aliases_and_accounts_together_are_rejected() {
    let yaml = "name: provider\ntype: xtream\nurl: http://a.net\nusername: root\npassword: pw\naliases:\n- name: one\n  url: http://a.net\n  username: one\n  password: pw\naccounts:\n- username: root\n  password: pw\n";
    let mut input: ConfigInputDto = serde_saphyr::from_str(yaml).expect("parse");
    // Two schemas for the same data: silently preferring one would drop subscriptions, so the
    // conflict is a configuration error rather than a conversion.
    let err = prepare_dto(&mut input).expect_err("aliases + accounts must fail");
    assert!(err.to_string().contains("both 'aliases' and 'accounts'"), "{err}");
}

#[test]
fn account_disabled_with_accounts_is_rejected() {
    let mut input = xtream_input();
    input.account_disabled = true;
    input.accounts = Some(vec![account("root_user", "pw")]);
    let err = prepare_dto(&mut input).expect_err("account_disabled + accounts must fail");
    assert!(err.to_string().contains("account_disabled"), "{err}");
}

#[test]
fn url_aliases_reject_provider_and_batch_schemes() {
    for lateral in ["provider://example", "batch://./accounts.csv"] {
        let mut input = xtream_input();
        input.url_aliases = Some(vec![lateral.intern()]);
        input.accounts = Some(vec![account("root_user", "pw")]);
        prepare_dto(&mut input).expect("prepare");
        // The invalid lateral is dropped, and the account keeps the address from `url`.
        assert!(
            input.url_aliases.as_ref().is_none_or(|aliases| aliases.iter().all(|url| !url.starts_with("provider://"))),
            "{lateral} survived"
        );
    }
}

#[test]
fn lateral_dedup_normalizes_port_slash_and_case() {
    assert_eq!(normalize_lateral_url("http://Host.NET:80/"), "http://host.net");
    assert_eq!(normalize_lateral_url("https://host.net:443"), "https://host.net");
    assert_eq!(normalize_lateral_url("http://host.net:8080/"), "http://host.net:8080");
    assert_eq!(normalize_lateral_url("http://host.net/path/"), "http://host.net/path");

    let mut input = xtream_input();
    input.url_aliases =
        Some(vec!["http://Host.NET:80/".intern(), "http://host.net".intern(), "http://other.net".intern()]);
    input.accounts = Some(vec![account("root_user", "pw")]);
    prepare_dto(&mut input).expect("prepare");
    assert_eq!(input.url_aliases.as_ref().expect("laterals").len(), 2, "the two spellings collapse");
}

#[test]
fn legacy_aliases_merge_by_field_not_by_position() {
    // One account repeated across three domains, with the copies disagreeing on the fields
    // that decide whether the account is usable. Keeping the first copy would disable the
    // account and lose the renewal.
    let yaml = "name: provider\ntype: xtream\nurl: http://a.net\nusername: root_user\npassword: root_pw\nmax_connections: 3\naliases:\n- name: root_user\n  url: http://a.net\n  username: root_user\n  password: root_pw\n  max_connections: 10\n  enabled: false\n  exp_date: 1790045999\n- name: root_user-1\n  url: http://b.net\n  username: root_user\n  password: root_pw\n  max_connections: 10\n  priority: 1\n  exp_date: 1792810799\n- name: root_user-2\n  url: http://c.net\n  username: root_user\n  password: root_pw\n  max_connections: 10\n";
    let mut input: ConfigInputDto = serde_saphyr::from_str(yaml).expect("parse");
    prepare_dto(&mut input).expect("prepare legacy config");

    let accounts = input.accounts.as_ref().expect("accounts");
    assert_eq!(accounts.len(), 1, "the three rows describe one subscription");
    let merged = &accounts[0];
    assert_eq!(merged.username.as_ref(), "root_user");
    assert!(merged.enabled, "enabled is any-true");
    assert_eq!(merged.exp_date, Some(1792810799), "exp_date is the newest renewal");
    assert_eq!(merged.priority, 1, "the declared non-default priority survives");
    assert_eq!(merged.max_connections, 10, "max_connections is the largest declared");
    assert_eq!(input.url_aliases.as_ref().expect("laterals").len(), 3);
    assert!(input.aliases.is_none(), "the legacy list is consumed");
}

#[test]
fn legacy_migration_keeps_a_narrower_lateral_list_per_account() {
    // `wide` reaches both domains; `narrow` only reaches the first. The union is both, but
    // `narrow` must not start trying the second domain.
    let yaml = "name: provider\ntype: xtream\nurl: http://a.net\nusername: wide\npassword: pw\naliases:\n- name: wide\n  url: http://b.net\n  username: wide\n  password: pw\n- name: narrow\n  url: http://a.net\n  username: narrow\n  password: pw\n";
    let mut input: ConfigInputDto = serde_saphyr::from_str(yaml).expect("parse");
    prepare_dto(&mut input).expect("prepare");

    let accounts = input.accounts.as_ref().expect("accounts");
    let narrow = accounts.iter().find(|account| account.username.as_ref() == "narrow").expect("narrow account");
    let laterals = input.account_laterals(narrow).expect("laterals");
    assert_eq!(laterals.len(), 1, "the narrow account keeps only the domain it used");
    assert_eq!(laterals[0].as_ref(), "http://a.net");

    let wide = accounts.iter().find(|account| account.username.as_ref() == "wide").expect("wide account");
    assert_eq!(input.account_laterals(wide).expect("laterals").len(), 2);
}

#[test]
fn account_without_address_and_without_laterals_is_rejected() {
    let mut input = xtream_input();
    input.url = String::new();
    input.accounts = Some(vec![account("root_user", "pw")]);
    let err = prepare_dto(&mut input).expect_err("no address anywhere must fail");
    assert!(err.to_string().contains("url_aliases"), "{err}");
}

#[test]
fn auto_epg_uses_account_one_and_the_primary_lateral() {
    let mut input = xtream_input();
    input.url = String::new();
    input.url_aliases = Some(vec!["http://lateral.net".intern()]);
    input.accounts = Some(vec![account("epg_user", "epg_pw")]);
    prepare_dto(&mut input).expect("prepare");

    let url = input.generate_auto_epg_url().expect("epg url");
    assert_eq!(url, "http://lateral.net/xmltv.php?username=epg_user&password=epg_pw");
}
