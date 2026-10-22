//! Migration from the legacy `aliases` list to `accounts` + `url_aliases`.
//!
//! Before accounts existed, one `aliases` entry was an account *and* an address: the same
//! subscription repeated once per provider domain. The normalized form keeps one account per
//! `username` and moves the addresses to the input's laterals.
//!
//! Folding duplicate accounts is lossy if done naively, because copies of the same account
//! disagree on `enabled`, `exp_date`, `priority` and `max_connections` (measured on real
//! configurations). Each field therefore merges by its own rule rather than "keep the first".

use super::{ConfigInputAccountDto, ConfigInputAliasDto, ConfigInputDto};
use crate::{
    error::TuliproxError,
    utils::{Internable, BATCH_SCHEME_PREFIX, PROVIDER_SCHEME_PREFIX},
};
use log::warn;
use std::{collections::HashMap, sync::Arc};

/// Normalized provider base URL used to decide whether two laterals are the same address.
///
/// Lowercases scheme and host, drops the default port and a trailing slash, and keeps a
/// non-empty path. Without this, `http://Host:80/` and `http://host` would count as two
/// laterals and rotate between the same server.
pub fn normalize_lateral_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.trim_end_matches('/').to_string();
    };
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        // A bare IPv6 address has several colons; only split a trailing numeric port.
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => (host, Some(port)),
        _ => (authority, None),
    };
    let host = host.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => Some("80"),
        "https" => Some("443"),
        _ => None,
    };
    let port = port.filter(|port| default_port != Some(*port));

    let mut normalized = String::with_capacity(trimmed.len());
    normalized.push_str(&scheme);
    normalized.push_str("://");
    normalized.push_str(&host);
    if let Some(port) = port {
        normalized.push(':');
        normalized.push_str(port);
    }
    normalized.push_str(path.trim_end_matches('/'));
    normalized
}

/// Merges two copies of the same account, field by field.
///
/// The rules follow what the copies mean rather than what position they have:
/// - `enabled` is `true` if any copy is enabled — disabling a usable subscription because an
///   older copy was disabled would silently shrink the pool.
/// - `exp_date` takes the largest value; a later date is a renewal, and the runtime disables an
///   account whose date has passed.
/// - `priority` keeps a declared non-default value (the lowest if several disagree), because the
///   default `0` in an earlier copy carries no intent.
/// - `max_connections` takes the largest non-zero value, since the copies describe the same quota.
/// - `password` and `stalker` keep the first non-empty value and warn on divergence.
fn merge_account(existing: &mut ConfigInputAccountDto, other: &ConfigInputAccountDto) {
    existing.enabled |= other.enabled;

    existing.exp_date = match (existing.exp_date, other.exp_date) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };

    if other.priority != 0 && (existing.priority == 0 || other.priority < existing.priority) {
        if existing.priority != 0 && existing.priority != other.priority {
            warn!(
                "account '{}': copies declare conflicting priorities ({} and {}); keeping {}",
                existing.username,
                existing.priority,
                other.priority,
                other.priority.min(existing.priority)
            );
        }
        existing.priority = other.priority;
    }

    if other.max_connections != 0 {
        if existing.max_connections != 0 && existing.max_connections != other.max_connections {
            warn!(
                "account '{}': copies declare conflicting max_connections ({} and {}); keeping {}",
                existing.username,
                existing.max_connections,
                other.max_connections,
                existing.max_connections.max(other.max_connections)
            );
        }
        existing.max_connections = existing.max_connections.max(other.max_connections);
    }

    match (existing.password.as_deref(), other.password.as_deref()) {
        (Some(a), Some(b)) if a != b => {
            warn!("account '{}': copies declare different passwords; keeping the first", existing.username);
        }
        (None, Some(password)) => existing.password = Some(password.to_string()),
        _ => {}
    }

    if existing.stalker.is_none() {
        existing.stalker.clone_from(&other.stalker);
    } else if other.stalker.is_some() && existing.stalker != other.stalker {
        warn!("account '{}': copies declare different stalker blocks; keeping the first", existing.username);
    }
}

impl ConfigInputDto {
    /// Converts a legacy `aliases` list into `accounts` + `url_aliases`.
    ///
    /// Runs after the legacy validation has accepted the file, so the migration only has to
    /// preserve data — it never has to re-litigate whether the file is valid. Batch inputs are
    /// excluded: their `aliases` list is a CSV projection that `prepare_batch` consumes later.
    pub(super) fn normalize_legacy_aliases(&mut self) {
        if self.accounts.is_some() || self.input_type.is_batch() {
            return;
        }
        let Some(legacy) = self.aliases.take() else { return };
        if legacy.is_empty() {
            self.aliases = None;
            return;
        }

        let root_url = self.url.trim().to_string();
        // Legacy rows repeat one account once per domain, so an account's reach is exactly the
        // set of `url` values in its rows. Collecting it per username is what lets the
        // migration keep an account on the domains it actually used.
        let mut reach: HashMap<String, Vec<Arc<str>>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        let root_username = self.username.as_deref().unwrap_or_default().trim().to_string();
        if !root_username.is_empty() {
            order.push(root_username.clone());
            let entry = reach.entry(root_username.clone()).or_default();
            push_lateral(entry, &root_url);
        }

        let mut folded: Vec<ConfigInputAccountDto> = Vec::with_capacity(legacy.len() + 1);
        for alias in legacy {
            let ConfigInputAliasDto {
                username,
                password,
                priority,
                max_connections,
                exp_date,
                enabled,
                stalker,
                url,
                name,
                ..
            } = alias;

            let display_name = name.trim().to_string();
            let account_username = username
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .unwrap_or_else(|| display_name.clone());
            if account_username.is_empty() {
                warn!("input '{}': skipping a legacy alias without username or name", self.name);
                continue;
            }
            if !display_name.is_empty() && !username.as_deref().is_some_and(|value| value.trim() == display_name) {
                warn!(
                    "input '{}': account '{}' was named '{}' in the legacy config; the account name is now its username",
                    self.name, account_username, display_name
                );
            }

            if !reach.contains_key(&account_username) {
                order.push(account_username.clone());
            }
            push_lateral(reach.entry(account_username.clone()).or_default(), &url);

            let account = ConfigInputAccountDto {
                id: 0,
                username: account_username.intern(),
                password,
                priority,
                max_connections,
                exp_date,
                enabled,
                stalker,
                url_aliases: None,
            };
            if let Some(existing) = folded.iter_mut().find(|candidate| candidate.username == account.username) {
                warn!(
                    "input '{}': account '{}' appears more than once in the legacy aliases; merging",
                    self.name, account.username
                );
                merge_account(existing, &account);
            } else {
                folded.push(account);
            }
        }

        // The root is account 1: it is the subscription the file already declared at input
        // level. Its row may also exist in the aliases list (the configuration does that to
        // give the root a second domain), in which case the two are already merged above.
        if !root_username.is_empty() {
            let mut root = ConfigInputAccountDto {
                id: 0,
                username: root_username.intern(),
                password: self.password.clone(),
                priority: self.priority,
                max_connections: self.max_connections,
                exp_date: self.exp_date,
                enabled: !self.account_disabled,
                stalker: self.stalker.clone(),
                url_aliases: None,
            };
            // The root's own row may also be present in the aliases list (the configuration
            // does that to give the root a second domain); fold it into the root so the
            // account keeps a single entry.
            if let Some(existing) = folded.iter().find(|candidate| candidate.username == root.username) {
                merge_account(&mut root, existing);
            }
            match folded.iter().position(|candidate| candidate.username == root.username) {
                Some(idx) => folded[idx] = root,
                None => folded.insert(0, root),
            }
        }

        // Union of every address, deduplicated by normalized form.
        let mut union: Vec<Arc<str>> = Vec::new();
        for username in &order {
            if let Some(urls) = reach.get(username) {
                for url in urls {
                    push_lateral(&mut union, url);
                }
            }
        }

        // An account that could only reach a subset keeps its own list, so the migration never
        // introduces a domain that the subscription has never used.
        for account in &mut folded {
            if let Some(urls) = reach.get(account.username.as_ref()) {
                let mut subset: Vec<Arc<str>> = Vec::new();
                for url in urls {
                    push_lateral(&mut subset, url);
                }
                if !subset.is_empty() && subset.len() < union.len() {
                    account.url_aliases = Some(subset);
                }
            }
        }

        self.url_aliases = (!union.is_empty()).then_some(union);
        self.accounts = Some(folded);
        self.derive_root_from_accounts();
    }

    /// Derives the root fields from account 1 in the new schema.
    ///
    /// The rest of the codebase still reads the root through `ConfigInputDto`/`ConfigInput`,
    /// so `accounts[0]` has to be projected onto those fields before validation runs.
    pub(super) fn derive_root_from_accounts(&mut self) {
        let Some(root) = self.accounts.as_ref().and_then(|accounts| accounts.first()) else { return };

        self.username = Some(root.username.as_ref().to_string());
        self.password.clone_from(&root.password);
        self.priority = root.priority;
        self.max_connections = root.max_connections;
        self.exp_date = root.exp_date;
        self.account_disabled = !root.enabled;
        if self.url.trim().is_empty() {
            let lateral = root.url_aliases.as_ref().or(self.url_aliases.as_ref());
            if let Some(first) = lateral.and_then(|aliases| aliases.first()) {
                self.url = first.as_ref().to_string();
            }
        }
    }

    /// Laterals an account may use: its own override, otherwise the input's list.
    pub fn account_laterals<'a>(&'a self, account: &'a ConfigInputAccountDto) -> Option<&'a [Arc<str>]> {
        account.url_aliases.as_deref().or(self.url_aliases.as_deref())
    }

    /// Rejects a file that mixes the two schemas for the same data.
    ///
    /// Runs before `normalize_legacy_aliases`, because that step consumes the legacy list:
    /// checking afterwards would let a conflicting file through with the legacy rows dropped.
    pub(super) fn validate_schema_exclusivity(&self) -> Result<(), TuliproxError> {
        if self.accounts.is_none() {
            return Ok(());
        }
        if self.aliases.as_ref().is_some_and(|aliases| !aliases.is_empty()) {
            return Err(TuliproxError::ConfigInput(format!(
                "input '{}' declares both 'aliases' and 'accounts'; use 'accounts' + 'url_aliases'",
                self.name
            )));
        }
        // `account_disabled` is the legacy spelling of "root account off". With accounts the
        // switch lives on the account itself, so the two must not be combined.
        if self.account_disabled {
            return Err(TuliproxError::ConfigInput(format!(
                "input '{}' declares 'account_disabled' together with 'accounts'; use accounts[0].enabled = false",
                self.name
            )));
        }
        // Without an address the accounts cannot reach the provider. Name both keys, because
        // the generic URL check that would catch this reports only `url`.
        let has_laterals = self.url_aliases.as_ref().is_some_and(|aliases| !aliases.is_empty());
        if !has_laterals && self.url.trim().is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "input '{}' declares accounts but no 'url' or 'url_aliases' to reach the provider",
                self.name
            )));
        }
        Ok(())
    }

    /// Validates the `accounts`/`url_aliases` block and assigns ids.
    pub(super) fn prepare_accounts(&mut self, start_index: u16) -> Result<u16, TuliproxError> {
        self.normalize_url_aliases_block();

        let Some(accounts) = self.accounts.as_mut() else { return Ok(start_index) };
        if accounts.is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "input '{}' declares an empty 'accounts' list",
                self.name
            )));
        }

        let input_type = self.input_type;
        let input_name = self.name.clone();
        let has_url_aliases = self.url_aliases.as_ref().is_some_and(|aliases| !aliases.is_empty());
        let mut current_index = start_index;
        let mut seen: Vec<Arc<str>> = Vec::with_capacity(accounts.len());
        for account in accounts.iter_mut() {
            current_index = account.prepare(current_index, &input_type, input_name.as_ref(), has_url_aliases)?;
            if seen.iter().any(|username| username == &account.username) {
                return Err(TuliproxError::ConfigInput(format!(
                    "duplicate account username '{}' in input '{}'",
                    account.username, self.name
                )));
            }
            seen.push(account.username.clone());
            if account.url_aliases.as_ref().is_some_and(|laterals| laterals.is_empty()) {
                return Err(TuliproxError::ConfigInput(format!(
                    "account '{}' of input '{}' declares an empty url_aliases list",
                    account.username, self.name
                )));
            }
            if account.url_aliases.is_some() && !has_url_aliases {
                return Err(TuliproxError::ConfigInput(format!(
                    "account '{}' of input '{}' declares url_aliases but the input does not",
                    account.username, self.name
                )));
            }
        }

        if !has_url_aliases && self.url.trim().is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "input '{}' declares accounts but no 'url' or 'url_aliases' to reach the provider",
                self.name
            )));
        }

        self.derive_root_from_accounts();
        Ok(current_index)
    }

    fn normalize_url_aliases_block(&mut self) {
        let Some(url_aliases) = self.url_aliases.take() else { return };
        let mut normalized: Vec<Arc<str>> = Vec::with_capacity(url_aliases.len());
        for url in url_aliases {
            let url = url.trim();
            if url.is_empty() {
                continue;
            }
            if url.starts_with(PROVIDER_SCHEME_PREFIX) || url.starts_with(BATCH_SCHEME_PREFIX) {
                warn!("input '{}': url_aliases accepts only http(s) base URLs, ignoring '{}'", self.name, url);
                continue;
            }
            push_lateral(&mut normalized, url);
        }
        self.url_aliases = (!normalized.is_empty()).then_some(normalized);
    }
}

fn push_lateral(laterals: &mut Vec<Arc<str>>, url: &str) {
    let url = url.trim();
    if url.is_empty() {
        return;
    }
    let canonical = normalize_lateral_url(url);
    if laterals.iter().any(|existing| normalize_lateral_url(existing.as_ref()) == canonical) {
        return;
    }
    laterals.push(url.trim_end_matches('/').intern());
}