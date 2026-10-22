use shared::{
    foundation::{
        compile_filter_draft_with_templates, import_filter_draft_with_templates, validate_filter_library,
        NumericOperator, StringOperator,
    },
    model::{
        new_managed_filter_rule_id, ConfigTargetFilterDto, FilterGroupOperator, FilterNode, FilterTypeValue, ItemField,
        ManagedFilterRule, PatternTemplate,
    },
};

pub type NodePath = Vec<usize>;

#[derive(Debug, Clone, PartialEq)]
pub struct TargetFilterBuilderState {
    pub processing: Option<FilterNode>,
    pub persist: Option<FilterNode>,
    pub rules: Vec<ManagedFilterRule>,
    pub templates: Vec<PatternTemplate>,
}

impl TargetFilterBuilderState {
    pub fn from_filter(
        filter: &ConfigTargetFilterDto,
        rules: Vec<ManagedFilterRule>,
        templates: Vec<PatternTemplate>,
    ) -> Self {
        Self {
            processing: import_filter_draft_with_templates(filter.processing.as_deref(), &rules, Some(&templates)),
            persist: import_filter_draft_with_templates(filter.persist.as_deref(), &rules, Some(&templates)),
            rules,
            templates,
        }
    }

    pub fn compile(&self) -> Result<ConfigTargetFilterDto, String> {
        let roots = self.roots_with_owners();
        validate_filter_library(&self.rules, &roots).map_err(|error| error.to_string())?;
        Ok(ConfigTargetFilterDto {
            processing: compile_filter_draft_with_templates(
                self.processing.as_ref(),
                &self.rules,
                Some(&self.templates),
            )
            .map_err(|error| error.to_string())?,
            persist: compile_filter_draft_with_templates(self.persist.as_ref(), &self.rules, Some(&self.templates))
                .map_err(|error| error.to_string())?,
            ..ConfigTargetFilterDto::default()
        })
    }

    pub fn roots_with_owners(&self) -> Vec<(String, FilterNode)> {
        let mut roots = Vec::with_capacity(2);
        if let Some(processing) = self.processing.as_ref() {
            roots.push(("target:processing".to_string(), processing.clone()));
        }
        if let Some(persist) = self.persist.as_ref() {
            roots.push(("target:persist".to_string(), persist.clone()));
        }
        roots
    }

    /// Rules whose match excludes an item at the persist stage. Paths for exclusion
    /// actions start with the index in this list.
    pub fn exclusion_rules(&self) -> Vec<FilterNode> {
        extract_persist_exclusions(self.persist.as_ref()).rules
    }

    /// The existing persist predicate that must still match for an item to survive.
    pub fn exclusion_base(&self) -> Option<FilterNode> {
        extract_persist_exclusions(self.persist.as_ref()).keep
    }

    pub fn rule_usage_count(&self, rule_id: &str) -> usize {
        validate_filter_library(&self.rules, &self.roots_with_owners())
            .ok()
            .and_then(|validation| validation.usages.get(rule_id).map(Vec::len))
            .unwrap_or_default()
    }
}

/// A persist predicate split into its untouched keep predicate and the rules
/// whose matches must be removed. Only top-level conjunctive negations are
/// extracted: negations inside an OR (or inside any other predicate) are not
/// independently removable without changing the predicate's meaning.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetFilterExclusions {
    pub keep: Option<FilterNode>,
    pub rules: Vec<FilterNode>,
}

pub fn extract_persist_exclusions(persist: Option<&FilterNode>) -> TargetFilterExclusions {
    let mut result = TargetFilterExclusions { keep: None, rules: Vec::new() };
    let Some(persist) = persist else { return result };
    result.keep = collect_conjunctive_exclusions(persist, &mut result.rules).0;
    result
}

/// The DSL parser introduces `All` wrapper groups for parenthesized expressions.
/// Walk conjunctions, but leave every keep-only subtree intact. A negation
/// inside a disjunction is not an independently removable exclusion.
fn collect_conjunctive_exclusions(node: &FilterNode, rules: &mut Vec<FilterNode>) -> (Option<FilterNode>, bool) {
    if let FilterNode::Group { operator: FilterGroupOperator::All, children } = node {
        let mut keep = Vec::new();
        let mut extracted = false;
        for child in children {
            let (remaining, found) = collect_conjunctive_exclusions(child, rules);
            extracted |= found;
            if let Some(remaining) = remaining {
                keep.push(remaining);
            }
        }
        if !extracted {
            return (Some(node.clone()), false);
        }
        let remaining = match keep.len() {
            0 => None,
            1 => Some(match keep.remove(0) {
                FilterNode::Group { operator: FilterGroupOperator::All, mut children } if children.len() == 1 => {
                    children.remove(0)
                }
                node => node,
            }),
            _ => Some(FilterNode::Group { operator: FilterGroupOperator::All, children: keep }),
        };
        (remaining, true)
    } else if append_exclusion_rules(node, rules) {
        (None, true)
    } else {
        (Some(node.clone()), false)
    }
}

fn unwrap_singleton_group(mut node: &FilterNode) -> &FilterNode {
    while let FilterNode::Group { children, .. } = node {
        if children.len() != 1 {
            break;
        }
        node = &children[0];
    }
    node
}

fn append_exclusion_rules(node: &FilterNode, rules: &mut Vec<FilterNode>) -> bool {
    let FilterNode::Not { node } = node else { return false };
    let excluded = unwrap_singleton_group(node);
    if let FilterNode::Group { operator: FilterGroupOperator::Any, children } = excluded {
        if children.is_empty() {
            return false;
        }
        rules.extend(children.iter().map(|child| unwrap_singleton_group(child).clone()));
    } else {
        rules.push(excluded.clone());
    }
    true
}

/// Builds `keep AND NOT(OR rules)` (or `NOT(single)`), retaining the keep
/// predicate as a whole subtree, including legacy raw expressions and refs.
pub fn compose_persist_exclusions(keep: Option<FilterNode>, rules: Vec<FilterNode>) -> Option<FilterNode> {
    let excluded = match rules.len() {
        0 => return keep,
        1 => rules.into_iter().next().expect("one exclusion rule"),
        _ => FilterNode::Group { operator: FilterGroupOperator::Any, children: rules },
    };
    let excluded = FilterNode::Not { node: Box::new(excluded) };
    Some(match keep {
        None => excluded,
        Some(keep) => FilterNode::Group {
            operator: FilterGroupOperator::All,
            children: vec![FilterNode::Group { operator: FilterGroupOperator::All, children: vec![keep] }, excluded],
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterStage {
    Processing,
    Persist,
}

impl FilterStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Processing => "processing",
            Self::Persist => "persist",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterBuilderAction {
    ReplaceRoot(FilterStage, Option<FilterNode>),
    ReplaceNode(FilterStage, NodePath, FilterNode),
    AddChild(FilterStage, NodePath, FilterNode),
    RemoveNode(FilterStage, NodePath),
    MoveNode(FilterStage, NodePath, isize),
    DuplicateNode(FilterStage, NodePath),
    WrapNode(FilterStage, NodePath, FilterGroupOperator),
    ToggleNot(FilterStage, NodePath),
    AddExclusion(FilterNode),
    ReplaceExclusion(NodePath, FilterNode),
    RemoveExclusion(NodePath),
    DuplicateExclusion(NodePath),
    MoveExclusion(NodePath, NodePath),
    AddRule(String),
    RenameRule(String, String),
    ReplaceRuleRoot(String, FilterNode),
    RemoveRule(String),
}

pub fn reduce_filter_builder(
    state: &TargetFilterBuilderState,
    action: FilterBuilderAction,
) -> Result<TargetFilterBuilderState, String> {
    let mut next = state.clone();
    match action {
        FilterBuilderAction::ReplaceRoot(stage, root) => *stage_root_mut(&mut next, stage) = root,
        FilterBuilderAction::ReplaceNode(stage, path, node) => {
            let root = stage_root_mut(&mut next, stage).as_mut().ok_or_else(|| "Filter is empty".to_string())?;
            *node_mut(root, &path).ok_or_else(|| "Filter node was not found".to_string())? = node;
        }
        FilterBuilderAction::AddChild(stage, path, node) => {
            let root = stage_root_mut(&mut next, stage).as_mut().ok_or_else(|| "Filter is empty".to_string())?;
            let target = node_mut(root, &path).ok_or_else(|| "Filter group was not found".to_string())?;
            let FilterNode::Group { children, .. } = target else {
                return Err("Children can only be added to a group".to_string());
            };
            children.push(node);
        }
        FilterBuilderAction::RemoveNode(stage, path) => remove_node(stage_root_mut(&mut next, stage), &path)?,
        FilterBuilderAction::MoveNode(stage, path, direction) => {
            move_node(stage_root_mut(&mut next, stage), &path, direction)?;
        }
        FilterBuilderAction::DuplicateNode(stage, path) => duplicate_node(stage_root_mut(&mut next, stage), &path)?,
        FilterBuilderAction::WrapNode(stage, path, operator) => {
            let root = stage_root_mut(&mut next, stage).as_mut().ok_or_else(|| "Filter is empty".to_string())?;
            let target = node_mut(root, &path).ok_or_else(|| "Filter node was not found".to_string())?;
            *target = FilterNode::Group { operator, children: vec![target.clone()] };
        }
        FilterBuilderAction::ToggleNot(stage, path) => {
            let root = stage_root_mut(&mut next, stage).as_mut().ok_or_else(|| "Filter is empty".to_string())?;
            let target = node_mut(root, &path).ok_or_else(|| "Filter node was not found".to_string())?;
            *target = match target.clone() {
                FilterNode::Not { node } => *node,
                node => FilterNode::Not { node: Box::new(node) },
            };
        }
        FilterBuilderAction::AddExclusion(node) => {
            let mut exclusions = extract_persist_exclusions(next.persist.as_ref());
            exclusions.rules.push(node);
            next.persist = compose_persist_exclusions(exclusions.keep, exclusions.rules);
        }
        FilterBuilderAction::ReplaceExclusion(path, node) => {
            let mut exclusions = extract_persist_exclusions(next.persist.as_ref());
            *exclusion_node_mut(&mut exclusions.rules, &path)? = node;
            next.persist = compose_persist_exclusions(exclusions.keep, exclusions.rules);
        }
        FilterBuilderAction::RemoveExclusion(path) => {
            let mut exclusions = extract_persist_exclusions(next.persist.as_ref());
            if path.len() == 1 {
                let index = exclusion_index(&exclusions.rules, &path)?;
                exclusions.rules.remove(index);
            } else {
                let (index, tail) = exclusion_path(&exclusions.rules, &path)?;
                let mut root = Some(exclusions.rules[index].clone());
                remove_node(&mut root, tail)?;
                exclusions.rules[index] = root.ok_or_else(|| "Exclusion rule cannot be empty".to_string())?;
            }
            next.persist = compose_persist_exclusions(exclusions.keep, exclusions.rules);
        }
        FilterBuilderAction::DuplicateExclusion(path) => {
            let mut exclusions = extract_persist_exclusions(next.persist.as_ref());
            if path.len() == 1 {
                let index = exclusion_index(&exclusions.rules, &path)?;
                let copy = exclusions.rules[index].clone();
                exclusions.rules.insert(index + 1, copy);
            } else {
                let (index, tail) = exclusion_path(&exclusions.rules, &path)?;
                let mut root = Some(exclusions.rules[index].clone());
                duplicate_node(&mut root, tail)?;
                exclusions.rules[index] = root.expect("duplicating an exclusion keeps its root");
            }
            next.persist = compose_persist_exclusions(exclusions.keep, exclusions.rules);
        }
        FilterBuilderAction::MoveExclusion(from, to) => {
            let mut exclusions = extract_persist_exclusions(next.persist.as_ref());
            let from_index = exclusion_index(&exclusions.rules, &from)?;
            let to_index = exclusion_index(&exclusions.rules, &to)?;
            if from.len() != 1 || to.len() != 1 {
                return Err("Only whole exclusion rules can be reordered".to_string());
            }
            let rule = exclusions.rules.remove(from_index);
            exclusions.rules.insert(to_index, rule);
            next.persist = compose_persist_exclusions(exclusions.keep, exclusions.rules);
        }
        FilterBuilderAction::AddRule(name) => {
            let id = new_managed_filter_rule_id();
            next.rules.push(ManagedFilterRule { id, name, root: default_condition() });
        }
        FilterBuilderAction::RenameRule(id, name) => {
            let rule = next.rules.iter_mut().find(|rule| rule.id == id).ok_or_else(|| "Rule not found".to_string())?;
            rule.name = name;
        }
        FilterBuilderAction::ReplaceRuleRoot(id, root) => {
            let rule = next.rules.iter_mut().find(|rule| rule.id == id).ok_or_else(|| "Rule not found".to_string())?;
            rule.root = root;
        }
        FilterBuilderAction::RemoveRule(id) => {
            if next.rule_usage_count(&id) > 0 {
                return Err("A shared rule cannot be deleted while it is in use".to_string());
            }
            next.rules.retain(|rule| rule.id != id);
        }
    }
    Ok(next)
}

fn exclusion_index(rules: &[FilterNode], path: &[usize]) -> Result<usize, String> {
    let Some(&index) = path.first() else { return Err("Exclusion path must start with a rule index".to_string()) };
    if index >= rules.len() {
        return Err("Exclusion rule was not found".to_string());
    }
    Ok(index)
}

fn exclusion_path<'a>(rules: &[FilterNode], path: &'a [usize]) -> Result<(usize, &'a [usize]), String> {
    Ok((exclusion_index(rules, path)?, &path[1..]))
}

fn exclusion_node_mut<'a>(rules: &'a mut [FilterNode], path: &[usize]) -> Result<&'a mut FilterNode, String> {
    let (index, tail) = exclusion_path(rules, path)?;
    node_mut(&mut rules[index], tail).ok_or_else(|| "Exclusion node was not found".to_string())
}

fn stage_root_mut(state: &mut TargetFilterBuilderState, stage: FilterStage) -> &mut Option<FilterNode> {
    match stage {
        FilterStage::Processing => &mut state.processing,
        FilterStage::Persist => &mut state.persist,
    }
}

pub fn default_condition() -> FilterNode {
    FilterNode::String { field: ItemField::Group, operator: StringOperator::Eq, value: String::new() }
}

pub fn default_group() -> FilterNode {
    FilterNode::Group { operator: FilterGroupOperator::All, children: vec![default_condition()] }
}

pub fn default_node_for_field(field: ItemField) -> FilterNode {
    match field {
        ItemField::Type => FilterNode::Type { value: FilterTypeValue::Live },
        ItemField::Chno | ItemField::Quality => FilterNode::Number { field, operator: NumericOperator::Eq, value: 0 },
        _ => FilterNode::String { field, operator: StringOperator::Eq, value: String::new() },
    }
}

pub fn node_mut<'a>(node: &'a mut FilterNode, path: &[usize]) -> Option<&'a mut FilterNode> {
    let Some((&head, tail)) = path.split_first() else {
        return Some(node);
    };
    match node {
        FilterNode::Group { children, .. } => children.get_mut(head).and_then(|child| node_mut(child, tail)),
        FilterNode::Not { node } if head == 0 => node_mut(node, tail),
        _ => None,
    }
}

fn remove_node(root: &mut Option<FilterNode>, path: &[usize]) -> Result<(), String> {
    if path.is_empty() {
        *root = None;
        return Ok(());
    }
    let parent_path = &path[..path.len() - 1];
    let index = *path.last().unwrap_or(&0);
    let root = root.as_mut().ok_or_else(|| "Filter is empty".to_string())?;
    let parent = node_mut(root, parent_path).ok_or_else(|| "Parent node was not found".to_string())?;
    match parent {
        FilterNode::Group { children, .. } if index < children.len() => {
            children.remove(index);
            Ok(())
        }
        FilterNode::Not { node } if index == 0 => {
            let replacement = (**node).clone();
            *parent = replacement;
            Ok(())
        }
        _ => Err("Filter node was not found".to_string()),
    }
}

fn move_node(root: &mut Option<FilterNode>, path: &[usize], direction: isize) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    let parent_path = &path[..path.len() - 1];
    let index = *path.last().unwrap_or(&0);
    let root = root.as_mut().ok_or_else(|| "Filter is empty".to_string())?;
    let parent = node_mut(root, parent_path).ok_or_else(|| "Parent group was not found".to_string())?;
    let FilterNode::Group { children, .. } = parent else {
        return Err("Only nodes inside a group can be moved".to_string());
    };
    let destination = index as isize + direction;
    if destination < 0 || destination >= children.len() as isize {
        return Ok(());
    }
    children.swap(index, destination as usize);
    Ok(())
}

fn duplicate_node(root: &mut Option<FilterNode>, path: &[usize]) -> Result<(), String> {
    if path.is_empty() {
        let value = root.clone().ok_or_else(|| "Filter is empty".to_string())?;
        *root = Some(FilterNode::Group { operator: FilterGroupOperator::All, children: vec![value.clone(), value] });
        return Ok(());
    }
    let parent_path = &path[..path.len() - 1];
    let index = *path.last().unwrap_or(&0);
    let root = root.as_mut().ok_or_else(|| "Filter is empty".to_string())?;
    let parent = node_mut(root, parent_path).ok_or_else(|| "Parent group was not found".to_string())?;
    let FilterNode::Group { children, .. } = parent else {
        return Err("Only nodes inside a group can be duplicated".to_string());
    };
    let value = children.get(index).cloned().ok_or_else(|| "Filter node was not found".to_string())?;
    children.insert(index + 1, value);
    Ok(())
}

pub fn string_operator_id(operator: StringOperator) -> &'static str {
    match operator {
        StringOperator::Eq => "eq",
        StringOperator::NotEq => "not_eq",
        StringOperator::Contains => "contains",
        StringOperator::StartsWith => "starts_with",
    }
}

pub fn parse_string_operator(value: &str) -> Option<StringOperator> {
    match value {
        "eq" => Some(StringOperator::Eq),
        "not_eq" => Some(StringOperator::NotEq),
        "contains" => Some(StringOperator::Contains),
        "starts_with" => Some(StringOperator::StartsWith),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_adds_moves_duplicates_and_removes_nested_nodes() {
        let state = TargetFilterBuilderState {
            processing: Some(FilterNode::Group {
                operator: FilterGroupOperator::All,
                children: vec![default_condition()],
            }),
            persist: None,
            rules: Vec::new(),
            templates: Vec::new(),
        };
        let state = reduce_filter_builder(
            &state,
            FilterBuilderAction::AddChild(FilterStage::Processing, vec![], default_node_for_field(ItemField::Chno)),
        )
        .unwrap();
        let state =
            reduce_filter_builder(&state, FilterBuilderAction::MoveNode(FilterStage::Processing, vec![1], -1)).unwrap();
        let state = reduce_filter_builder(&state, FilterBuilderAction::DuplicateNode(FilterStage::Processing, vec![0]))
            .unwrap();
        let state =
            reduce_filter_builder(&state, FilterBuilderAction::RemoveNode(FilterStage::Processing, vec![1])).unwrap();
        let FilterNode::Group { children, .. } = state.processing.unwrap() else { panic!("group") };
        assert_eq!(children.len(), 2);
        assert!(matches!(children[0], FilterNode::Number { field: ItemField::Chno, .. }));
    }

    #[test]
    fn reducer_toggles_not_without_nesting_repeated_not_nodes() {
        let state = TargetFilterBuilderState {
            processing: Some(default_condition()),
            persist: None,
            rules: Vec::new(),
            templates: Vec::new(),
        };
        let state =
            reduce_filter_builder(&state, FilterBuilderAction::ToggleNot(FilterStage::Processing, vec![])).unwrap();
        assert!(matches!(state.processing, Some(FilterNode::Not { .. })));
        let state =
            reduce_filter_builder(&state, FilterBuilderAction::ToggleNot(FilterStage::Processing, vec![])).unwrap();
        assert!(matches!(state.processing, Some(FilterNode::String { .. })));
    }

    #[test]
    fn real_legacy_target_filter_compiles_with_loaded_templates() {
        let filter = ConfigTargetFilterDto {
            processing: Some("!NO_24H! AND !NO_ADULT! AND !NO_H265!".to_string()),
            ..ConfigTargetFilterDto::default()
        };
        let templates = ["NO_24H", "NO_ADULT", "NO_H265"]
            .into_iter()
            .map(|name| PatternTemplate {
                name: name.to_string(),
                value: shared::model::TemplateValue::Single(r#"Group ~ ".*""#.to_string()),
                filter_rule: None,
                placeholder: String::new(),
            })
            .collect();
        let state = TargetFilterBuilderState::from_filter(&filter, Vec::new(), templates);

        let processing = state.processing.as_ref().expect("visual tree");
        let FilterNode::Group { children, .. } = processing else {
            panic!("legacy filter should import as a visual group, got {processing:?}")
        };
        assert_eq!(children.len(), 3);
        assert!(matches!(&children[0], FilterNode::LegacyReference { template } if template == "NO_24H"));
        assert_eq!(state.compile().unwrap().processing.as_deref(), Some(r"(!NO_24H! AND !NO_ADULT! AND !NO_H265!)"));
    }

    fn state_with_persist(persist: Option<FilterNode>) -> TargetFilterBuilderState {
        TargetFilterBuilderState {
            processing: Some(default_node_for_field(ItemField::Chno)),
            persist,
            rules: Vec::new(),
            templates: Vec::new(),
        }
    }

    fn named_rule(value: &str) -> FilterNode {
        FilterNode::String { field: ItemField::Group, operator: StringOperator::Eq, value: value.to_string() }
    }

    #[test]
    fn exclusion_roundtrip_and_reorder_preserve_processing() {
        let first = named_rule("first");
        let second = named_rule("second");
        let third = named_rule("third");
        let initial = state_with_persist(None);
        let one = reduce_filter_builder(&initial, FilterBuilderAction::AddExclusion(first.clone())).unwrap();
        assert_eq!(one.persist, Some(FilterNode::Not { node: Box::new(first.clone()) }));
        let two = reduce_filter_builder(&one, FilterBuilderAction::AddExclusion(second.clone())).unwrap();
        assert_eq!(two.exclusion_rules(), vec![first.clone(), second.clone()]);
        assert!(
            matches!(two.persist.as_ref(), Some(FilterNode::Not { node }) if matches!(node.as_ref(), FilterNode::Group { operator: FilterGroupOperator::Any, .. }))
        );
        let three = reduce_filter_builder(&two, FilterBuilderAction::AddExclusion(third.clone())).unwrap();
        let moved = reduce_filter_builder(&three, FilterBuilderAction::MoveExclusion(vec![0], vec![2])).unwrap();
        assert_eq!(moved.exclusion_rules(), vec![second.clone(), third.clone(), first.clone()]);
        let duplicate = reduce_filter_builder(&moved, FilterBuilderAction::DuplicateExclusion(vec![1])).unwrap();
        assert_eq!(duplicate.exclusion_rules(), vec![second.clone(), third.clone(), third, first.clone()]);
        let replaced =
            reduce_filter_builder(&duplicate, FilterBuilderAction::ReplaceExclusion(vec![0], first.clone())).unwrap();
        let removed = reduce_filter_builder(&replaced, FilterBuilderAction::RemoveExclusion(vec![3])).unwrap();
        assert_eq!(removed.exclusion_rules(), vec![first, named_rule("third"), named_rule("third")]);
        assert_eq!(removed.processing, initial.processing);
        let compiled = removed.compile().unwrap();
        let reimported = TargetFilterBuilderState::from_filter(&compiled, vec![], vec![]);
        assert_eq!(reimported.exclusion_rules(), removed.exclusion_rules());
        assert_eq!(reimported.compile().unwrap().processing, compiled.processing);
        // The DSL importer may add harmless grouping parentheses on recompile.
        assert!(reimported.compile().unwrap().persist.unwrap().contains("NOT"));
    }

    #[test]
    fn legacy_keep_predicate_is_preserved_and_not_reinterpreted() {
        let keep = FilterNode::Raw { expression: r#"(Group ~ "Sports" OR Name ~ "News")"#.to_string() };
        let processing = FilterNode::LegacyReference { template: "PROCESS".to_string() };
        let mut state = state_with_persist(Some(keep.clone()));
        state.processing = Some(processing.clone());
        let added = reduce_filter_builder(&state, FilterBuilderAction::AddExclusion(named_rule("blocked"))).unwrap();
        assert_eq!(added.exclusion_base(), Some(keep.clone()));
        assert_eq!(added.processing, Some(processing));
        assert_eq!(added.persist, compose_persist_exclusions(Some(keep.clone()), vec![named_rule("blocked")]));
        let saved = compile_filter_draft_with_templates(added.persist.as_ref(), &[], None).unwrap().unwrap();
        assert!(saved.contains("(Group ~ \"Sports\" OR Name ~ \"News\")"));
        assert!(saved.contains("AND NOT"));
        let removed = reduce_filter_builder(&added, FilterBuilderAction::RemoveExclusion(vec![0])).unwrap();
        assert_eq!(removed.persist, Some(keep));
        assert_eq!(removed.processing, state.processing);
    }

    #[test]
    fn only_top_level_conjunctive_negations_are_exclusions() {
        let nested = FilterNode::Group {
            operator: FilterGroupOperator::Any,
            children: vec![named_rule("allow"), FilterNode::Not { node: Box::new(named_rule("not-independent")) }],
        };
        let existing = FilterNode::Group {
            operator: FilterGroupOperator::All,
            children: vec![nested.clone(), FilterNode::Not { node: Box::new(named_rule("blocked")) }],
        };
        let state = state_with_persist(Some(existing));
        assert_eq!(state.exclusion_base(), Some(nested.clone()));
        assert_eq!(state.exclusion_rules(), vec![named_rule("blocked")]);
        let state = reduce_filter_builder(&state, FilterBuilderAction::AddExclusion(named_rule("extra"))).unwrap();
        assert_eq!(state.exclusion_base(), Some(nested));
        assert_eq!(state.exclusion_rules(), vec![named_rule("blocked"), named_rule("extra")]);
        let opaque = FilterNode::Not {
            node: Box::new(FilterNode::Group {
                operator: FilterGroupOperator::All,
                children: vec![named_rule("a"), named_rule("b")],
            }),
        };
        // NOT(AND ...) denotes one exclusion rule, not two independent exclusions.
        assert_eq!(
            extract_persist_exclusions(Some(&opaque)).rules,
            vec![FilterNode::Group {
                operator: FilterGroupOperator::All,
                children: vec![named_rule("a"), named_rule("b")],
            }]
        );
    }

    #[test]
    fn exclusion_paths_validate_indices_and_nested_nodes() {
        let initial = state_with_persist(compose_persist_exclusions(
            None,
            vec![FilterNode::Group {
                operator: FilterGroupOperator::All,
                children: vec![named_rule("original"), named_rule("other")],
            }],
        ));
        for action in [
            FilterBuilderAction::RemoveExclusion(vec![]),
            FilterBuilderAction::ReplaceExclusion(vec![1], named_rule("x")),
            FilterBuilderAction::ReplaceExclusion(vec![0, 9], named_rule("x")),
            FilterBuilderAction::DuplicateExclusion(vec![0, 9]),
            FilterBuilderAction::RemoveExclusion(vec![0, 9]),
            FilterBuilderAction::MoveExclusion(vec![0, 0], vec![0]),
            FilterBuilderAction::MoveExclusion(vec![0], vec![1]),
        ] {
            assert!(reduce_filter_builder(&initial, action).is_err());
        }
        let replaced =
            reduce_filter_builder(&initial, FilterBuilderAction::ReplaceExclusion(vec![0, 0], named_rule("x")))
                .unwrap();
        assert_eq!(
            replaced.exclusion_rules(),
            vec![FilterNode::Group {
                operator: FilterGroupOperator::All,
                children: vec![named_rule("x"), named_rule("other")],
            }]
        );
    }

    #[test]
    fn exclusions_with_managed_and_legacy_template_references_compile() {
        let rule = ManagedFilterRule { id: "one".to_string(), name: "One".to_string(), root: named_rule("shared") };
        let template = PatternTemplate {
            name: "LEGACY".to_string(),
            value: shared::model::TemplateValue::Single(r#"Group ~ "Sports""#.to_string()),
            filter_rule: None,
            placeholder: String::new(),
        };
        let keep = FilterNode::LegacyReference { template: "LEGACY".to_string() };
        let mut state = state_with_persist(Some(keep.clone()));
        state.rules = vec![rule.clone()];
        state.templates = vec![template.clone()];
        let added = reduce_filter_builder(
            &state,
            FilterBuilderAction::AddExclusion(FilterNode::Reference { rule_id: rule.id.clone() }),
        )
        .unwrap();
        assert_eq!(added.exclusion_base(), Some(keep));
        assert_eq!(added.rule_usage_count("one"), 1);
        let compiled = added.compile().unwrap();
        let imported = TargetFilterBuilderState::from_filter(&compiled, vec![rule], vec![template]);
        assert_eq!(imported.exclusion_rules(), added.exclusion_rules());
        assert!(imported.compile().is_ok());
    }

    #[test]
    fn referenced_rule_cannot_be_removed() {
        let rule = ManagedFilterRule { id: "one".to_string(), name: "One".to_string(), root: default_condition() };
        let state = TargetFilterBuilderState {
            processing: Some(FilterNode::Reference { rule_id: "one".to_string() }),
            persist: None,
            rules: vec![rule],
            templates: Vec::new(),
        };
        assert!(reduce_filter_builder(&state, FilterBuilderAction::RemoveRule("one".to_string())).is_err());
    }
}
