use super::{stalker::prepare_stalker_config, InputType};
use crate::{
    defaults::{default_as_true, is_true, is_zero_i16, is_zero_u16},
    error::TuliproxError,
    model::StalkerInputConfigDto,
    utils::{arc_str_serde, deserialize_timestamp, is_blank_optional_string, Internable},
};
use std::sync::Arc;

/// One subscription account of an input.
///
/// The account replaces the old `aliases` entry: it carries only credentials and the
/// per-account quota. The provider addresses live in the input's `url_aliases` and are shared
/// by every account — an account cannot narrow or replace them. The account has no `name` —
/// its identity is the `username` — and no `url`.
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
        }
    }
}

impl ConfigInputAccountDto {
    /// Validates and normalizes one account.
    ///
    /// The account has no address of its own: it reaches the provider through the input's
    /// laterals, so the URL-shaped M3U credential extraction does not apply and credentials
    /// must come from the account itself. `input_name` is used for error messages and for the
    /// Stalker device identity check, which names the failing portal.
    pub fn prepare(&mut self, index: u16, input_type: &InputType, input_name: &str) -> Result<u16, TuliproxError> {
        self.id = index + 1;
        self.username = self.username.trim().intern();
        if self.username.is_empty() {
            return Err(TuliproxError::ConfigInput(format!("username for account is mandatory (input: {input_name})")));
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
            InputType::M3u | InputType::M3uBatch | InputType::Stalker | InputType::StalkerBatch => {
                // These types can address the provider through the input's laterals; their
                // credential rules are input-level and were already validated.
            }
            InputType::Library | InputType::Emby | InputType::Jellyfin | InputType::Plex | InputType::Staged => {
                return Err(TuliproxError::ConfigInput(format!(
                    "input type {} does not support accounts (input: {input_name})",
                    input_type
                )));
            }
        }

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
}
