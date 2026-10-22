use super::{stalker::prepare_stalker_config, InputType};
use crate::{
    defaults::{default_as_true, is_true, is_zero_i16, is_zero_u16},
    error::TuliproxError,
    model::StalkerInputConfigDto,
    utils::{
        arc_str_serde, deserialize_timestamp, is_blank_optional_string, trim_last_slash, Internable,
        BATCH_SCHEME_PREFIX, PROVIDER_SCHEME_PREFIX,
    },
};
use log::warn;
use std::sync::Arc;

/// One subscription account of an input.
///
/// The account replaces the old `aliases` entry: it carries the credentials and the
/// per-account quota, while the provider addresses live in the input's `url_aliases`
/// (optionally narrowed per account). The account has no `name` — its identity is the
/// `username`, and no `url` — the address is a lateral of the owning input.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputAccountDto {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub id: u16,
    #[serde(with = "arc_str_serde")]
    pub username: Arc<str>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_i16")]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default, deserialize_with = "deserialize_timestamp", skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalker: Option<StalkerInputConfigDto>,
    /// Narrows the input's lateral list for this account. Replaces (never merges with)
    /// `ConfigInputDto::url_aliases`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_aliases: Option<Vec<Arc<str>>>,
}

/// The account has no `name` — its identity is the `username`, and no `url` — the address is a
/// lateral of the owning input.
impl Default for ConfigInputAccountDto {
    fn default() -> Self {
        Self {
            id: 0,
            username: "".intern(),
            password: None,
            priority: 0,
            max_connections: 0,
            exp_date: None,
            // Must match the serde default: a hand-built account behaves like a parsed one.
            enabled: default_as_true(),
            stalker: None,
            url_aliases: None,
        }
    }
}

impl ConfigInputAccountDto {
    /// Validates and normalizes one account.
    ///
    /// `input_has_url_aliases` tells whether the owning input declares laterals: a
    /// lateral-backed account has no address of its own, so only the batch credential
    /// rules apply to it. `input_name` is used for error messages and for the Stalker
    /// device identity check, which names the failing portal.
    pub fn prepare(
        &mut self,
        index: u16,
        input_type: &InputType,
        input_name: &str,
        input_has_url_aliases: bool,
    ) -> Result<u16, TuliproxError> {
        self.id = index + 1;
        self.username = self.username.trim().intern();
        if self.username.is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "username for account is mandatory (input: {input_name})"
            )));
        }
        self.password = crate::utils::get_trimmed_string(self.password.as_deref());

        match input_type {
            InputType::Xtream | InputType::XtreamBatch => {
                if self.username.is_empty() || self.password.is_none() {
                    return Err(TuliproxError::ConfigInput(format!(
                        "for input type xtream: username and password are mandatory (input: {input_name}, account: {})",
                        self.username
                    )));
                }
            }
            // An account carries no URL, so the URL-shaped M3U credential extraction does
            // not apply; credentials must come from the account itself.
            InputType::M3u | InputType::M3uBatch => {
                if !input_has_url_aliases {
                    return Err(TuliproxError::ConfigInput(format!(
                        "m3u account requires the input to declare url_aliases (input: {input_name}, account: {})",
                        self.username
                    )));
                }
            }
            InputType::Stalker | InputType::StalkerBatch => {
                // Stalker authenticates with a MAC, credentials, or both; the identity
                // check that needs both sources lives in `prepare_stalker_config`.
            }
            InputType::Library
            | InputType::Emby
            | InputType::Jellyfin
            | InputType::Plex
            | InputType::Staged => {
                return Err(TuliproxError::ConfigInput(format!(
                    "input type {} does not support accounts (input: {input_name})",
                    input_type
                )));
            }
        }

        self.normalize_url_aliases(input_name)?;

        prepare_stalker_config(
            self.username.as_ref(),
            input_type,
            &mut self.stalker,
            true,
            Some(self.username.as_ref()),
            self.password.as_deref(),
        )?;

        Ok(self.id)
    }

    fn normalize_url_aliases(&mut self, input_name: &str) -> Result<(), TuliproxError> {
        let Some(url_aliases) = self.url_aliases.as_mut() else {
            return Ok(());
        };
        let mut normalized: Vec<Arc<str>> = Vec::with_capacity(url_aliases.len());
        for url in url_aliases.drain(..) {
            let url = url.trim();
            if url.is_empty() {
                continue;
            }
            if url.starts_with(PROVIDER_SCHEME_PREFIX) || url.starts_with(BATCH_SCHEME_PREFIX) {
                return Err(TuliproxError::ConfigInput(format!(
                    "account url_aliases accepts only http(s) base URLs, not '{url}' (input: {input_name}, account: {})",
                    self.username
                )));
            }
            let url = trim_last_slash(url).into_owned();
            if !normalized.iter().any(|existing| existing.as_ref() == url.as_str()) {
                normalized.push(url.intern());
            }
        }
        if normalized.is_empty() {
            // An empty list means "no override": fall back to the input's laterals rather
            // than leaving the account without any address.
            warn!(
                "account '{}' of input '{}' declares an empty url_aliases list; using the input's url_aliases",
                self.username, input_name
            );
            self.url_aliases = None;
        } else {
            self.url_aliases = Some(normalized);
        }
        Ok(())
    }
}