use crate::model::config::ConfigInput;
use shared::model::provider_saturation::{CapacityGroup, ProviderSlot};
use std::{collections::HashMap, sync::Arc};

#[allow(clippy::implicit_hasher)]
pub fn build_provider_slots(
    inputs: &[Arc<ConfigInput>],
    connections: &HashMap<Arc<str>, usize>,
) -> Vec<ProviderSlot<Arc<str>>> {
    let mut slots = Vec::with_capacity(inputs.len());
    for input in inputs {
        // Disabled members have no lineup and cannot carry connections.
        if !input.enabled {
            continue;
        }
        let current = connections.get(&input.name).copied().unwrap_or(0);
        slots.push(ProviderSlot { name: input.name.clone(), max_connections: input.max_connections, current });
        push_aliases(&mut slots, input, connections);
    }
    slots
}

impl CapacityGroup for ConfigInput {
    fn name(&self) -> &Arc<str> {
        &self.name
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    /// The key the pool uses for this account.
    ///
    /// The account's own `name` is its username, and the same subscription may legitimately be
    /// declared by more than one input. Only the qualified `<input>/<username>` is unique, so
    /// that is what the capacity grouping keys on.
    fn alias_names(&self) -> impl Iterator<Item = Arc<str>> {
        self.aliases
            .iter()
            .flatten()
            .filter(|alias| alias.enabled)
            .map(|alias| alias.account_name(self.name.as_ref()))
    }
}

fn push_aliases(
    slots: &mut Vec<ProviderSlot<Arc<str>>>,
    input: &ConfigInput,
    connections: &HashMap<Arc<str>, usize>,
) {
    let Some(aliases) = input.aliases.as_deref() else { return };
    for alias in aliases {
        if !alias.enabled {
            continue;
        }
        let name = alias.account_name(input.name.as_ref());
        let current = connections.get(&name).copied().unwrap_or(0);
        slots.push(ProviderSlot { name, max_connections: alias.max_connections, current });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConfigInputAlias;
    use shared::model::provider_saturation::{build_group_lookup, is_exhausted};

    fn input(name: &str, max: u16) -> Arc<ConfigInput> {
        Arc::new(ConfigInput { name: Arc::from(name), max_connections: max, enabled: true, ..ConfigInput::default() })
    }

    #[test]
    fn exhausted_when_all_groups_full() {
        let inputs = vec![input("a", 1), input("b", 2)];
        let mut conns = HashMap::new();
        conns.insert(Arc::from("a"), 1usize);
        conns.insert(Arc::from("b"), 2usize);
        let slots = build_provider_slots(&inputs, &conns);
        let groups = build_group_lookup(&inputs);
        assert!(is_exhausted(slots, &groups));
    }

    #[test]
    fn ready_when_any_group_has_capacity() {
        let inputs = vec![input("a", 1), input("b", 5)];
        let mut conns = HashMap::new();
        conns.insert(Arc::from("a"), 1usize);
        conns.insert(Arc::from("b"), 0usize);
        let slots = build_provider_slots(&inputs, &conns);
        let groups = build_group_lookup(&inputs);
        assert!(!is_exhausted(slots, &groups));
    }

    #[test]
    fn disabled_members_offer_no_capacity() {
        let mut full_input =
            ConfigInput { name: Arc::from("a"), max_connections: 1, enabled: true, ..ConfigInput::default() };
        full_input.aliases = Some(vec![ConfigInputAlias {
            id: 2,
            name: Arc::from("a-off"),
            url: "http://alias".to_string(),
            username: None,
            password: None,
            priority: 0,
            max_connections: 5,
            exp_date: None,
            enabled: false,
            stalker: None,
            ..Default::default()
        }]);
        // The disabled alias and the disabled input must not add spare capacity.
        let disabled_input =
            Arc::new(ConfigInput { name: Arc::from("off"), max_connections: 9, ..ConfigInput::default() });
        let inputs = vec![Arc::new(full_input), disabled_input];
        let mut conns = HashMap::new();
        conns.insert(Arc::from("a"), 1usize);
        let slots = build_provider_slots(&inputs, &conns);
        let groups = build_group_lookup(&inputs);
        assert!(is_exhausted(slots, &groups));
    }

    /// The same subscription declared by two inputs yields two members, not one overwritten key.
    ///
    /// Both the slot names and the group keys are the pool's qualified `<input>/<username>`, so the
    /// capacity of each input stays attributable to it.
    #[test]
    fn accounts_with_the_same_username_in_two_inputs_stay_separate() {
        let make = |input_name: &'static str| {
            Arc::new(ConfigInput {
                name: Arc::from(input_name),
                max_connections: 1,
                enabled: true,
                aliases: Some(vec![ConfigInputAlias {
                    name: Arc::from("shared"),
                    username: Some("shared".to_string()),
                    enabled: true,
                    max_connections: 2,
                    ..Default::default()
                }]),
                ..ConfigInput::default()
            })
        };
        let inputs = vec![make("one"), make("two")];

        let slots = build_provider_slots(&inputs, &HashMap::new());
        let groups = build_group_lookup(&inputs);

        assert!(slots.iter().any(|slot| slot.name.as_ref() == "one/shared"));
        assert!(slots.iter().any(|slot| slot.name.as_ref() == "two/shared"));
        assert_eq!(groups.get("one/shared").map(Arc::as_ref), Some("one"));
        assert_eq!(groups.get("two/shared").map(Arc::as_ref), Some("two"));
    }
}
