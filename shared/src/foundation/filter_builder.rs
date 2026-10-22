use crate::{
    error::TuliproxError,
    foundation::{get_filter, BinaryOperator, Filter, StringOperator, UnaryOperator},
    model::{
        managed_filter_rule_template_name, FilterGroupOperator, FilterNode, FilterTypeValue, ItemField,
        ManagedFilterRule, PatternTemplate, PlaylistItemType, TemplateValue,
    },
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterBuilderError {
    pub path: Vec<usize>,
    pub message: String,
}

impl FilterBuilderError {
    fn root(message: impl Into<String>) -> Self {
        Self { path: Vec::new(), message: message.into() }
    }

    fn at(path: &[usize], message: impl Into<String>) -> Self {
        Self { path: path.to_vec(), message: message.into() }
    }
}

impl std::fmt::Display for FilterBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(
                f,
                "{} (node {})",
                self.message,
                self.path.iter().map(usize::to_string).collect::<Vec<_>>().join(".")
            )
        }
    }
}

impl std::error::Error for FilterBuilderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterRuleUsage {
    pub owner: String,
    pub path: Vec<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterLibraryValidation {
    pub usages: HashMap<String, Vec<FilterRuleUsage>>,
}

pub fn managed_filter_rules(templates: Option<&[PatternTemplate]>) -> Vec<ManagedFilterRule> {
    templates.into_iter().flatten().filter_map(PatternTemplate::managed_filter_rule).collect()
}

pub fn managed_filter_rule_templates(rules: &[ManagedFilterRule]) -> Result<Vec<PatternTemplate>, FilterBuilderError> {
    validate_filter_library(rules, &[])?;
    rules
        .iter()
        .map(|rule| {
            let value = compile_filter_node(&rule.root, rules)?;
            Ok(PatternTemplate {
                name: rule.template_name(),
                value: TemplateValue::Single(value),
                filter_rule: Some(rule.into()),
                placeholder: rule.placeholder(),
            })
        })
        .collect()
}

pub fn import_filter_draft(expression: Option<&str>, rules: &[ManagedFilterRule]) -> Option<FilterNode> {
    import_filter_draft_with_templates(expression, rules, None)
}

/// Imports a filter expression into the visual tree. Known placeholders — both
/// managed shared rules and legacy `!TEMPLATE!` catalog entries — become linked
/// reference nodes; only unknown placeholders or malformed DSL fall back to a
/// verbatim raw block.
pub fn import_filter_draft_with_templates(
    expression: Option<&str>,
    rules: &[ManagedFilterRule],
    templates: Option<&[PatternTemplate]>,
) -> Option<FilterNode> {
    let expression = expression.map(str::trim).filter(|value| !value.is_empty())?;
    if let Some(rule_id) = exact_managed_reference(expression, rules) {
        return Some(FilterNode::Reference { rule_id });
    }

    let (parseable, sentinels) = replace_placeholder_references(expression, rules, templates);
    if contains_template_placeholder(&parseable) {
        return Some(FilterNode::Raw { expression: expression.to_string() });
    }

    match get_filter(&parseable, None) {
        Ok(filter) => {
            let mut node = filter_to_node(&filter);
            if restore_placeholder_references(&mut node, &sentinels) {
                Some(node)
            } else {
                Some(FilterNode::Raw { expression: expression.to_string() })
            }
        }
        Err(_) => Some(FilterNode::Raw { expression: expression.to_string() }),
    }
}

pub fn compile_filter_draft(
    draft: Option<&FilterNode>,
    rules: &[ManagedFilterRule],
) -> Result<Option<String>, FilterBuilderError> {
    compile_filter_draft_with_templates(draft, rules, None)
}

pub fn compile_filter_draft_with_templates(
    draft: Option<&FilterNode>,
    rules: &[ManagedFilterRule],
    templates: Option<&[PatternTemplate]>,
) -> Result<Option<String>, FilterBuilderError> {
    let Some(root) = draft else {
        return Ok(None);
    };
    validate_filter_library(rules, &[])?;
    let mut legacy_references = Vec::new();
    collect_legacy_references(root, &mut legacy_references);
    if !legacy_references.is_empty() {
        let available = templates
            .into_iter()
            .flatten()
            .filter(|template| !template.is_managed_filter_rule())
            .map(|template| template.name.as_str())
            .collect::<HashSet<_>>();
        for template in &legacy_references {
            if !available.contains(template.as_str()) {
                return Err(FilterBuilderError::root(format!("Unknown template '{template}'")));
            }
        }
    }
    let expression = compile_filter_node(root, rules)?;
    let prepared_templates = prepare_filter_builder_templates(rules, templates)?;
    get_filter(&expression, (!prepared_templates.is_empty()).then_some(prepared_templates.as_slice()))
        .map_err(|error| FilterBuilderError::root(error.to_string()))?;
    Ok(Some(expression))
}

fn collect_legacy_references(node: &FilterNode, out: &mut Vec<String>) {
    match node {
        FilterNode::Group { children, .. } => children.iter().for_each(|child| collect_legacy_references(child, out)),
        FilterNode::Not { node } => collect_legacy_references(node, out),
        FilterNode::LegacyReference { template } => out.push(template.clone()),
        _ => {}
    }
}

pub fn prepare_filter_builder_templates(
    rules: &[ManagedFilterRule],
    templates: Option<&[PatternTemplate]>,
) -> Result<Vec<PatternTemplate>, FilterBuilderError> {
    let mut prepared_templates = templates
        .into_iter()
        .flatten()
        .filter(|template| !template.is_managed_filter_rule())
        .cloned()
        .collect::<Vec<_>>();
    prepared_templates.extend(managed_filter_rule_templates_unchecked(rules)?);
    super::prepare_templates(&mut prepared_templates).map_err(|error| FilterBuilderError::root(error.to_string()))
}

pub fn compile_filter_node(node: &FilterNode, rules: &[ManagedFilterRule]) -> Result<String, FilterBuilderError> {
    let mut path = Vec::new();
    compile_node(node, rules, &mut path)
}

/// A category and its explicitly kept channel names for visual selection compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualCategoryRule {
    pub name: String,
    pub is_enabled: bool,
    pub kept_channel_names: Vec<String>,
}

/// Compiles visual category and channel selections into a persistent, precise
/// regex/DSL filter.
///
/// Rules:
/// - Unchecked categories are removed entirely (`Group = "Cat"`).
/// - Checked categories keep only their explicitly listed channels (`Group = "Cat" AND NOT (Caption IN [...])`).
///   Future new channels in these known categories are NOT included by default.
/// - Genuinely new categories from the source do not match any category removal clause, so they
///   are naturally included by default.
/// - If all categories and channels are kept (no removals), returns `existing_filter`.
pub fn compile_visual_rules_to_filter_dsl(
    existing_filter: Option<&str>,
    rules: &[VisualCategoryRule],
) -> Result<Option<String>, FilterBuilderError> {
    // `!` starts a template placeholder (`!NAME!`) and is substituted textually
    // BEFORE the parser sees quoted strings, so any name containing `!` must be
    // matched with a regex that encodes it as `\x21` instead of a bare literal.
    fn has_bang(value: &str) -> bool {
        value.contains('!')
    }

    fn escape_dsl_string(value: &str) -> String {
        value.replace('"', "\\\"")
    }

    fn regex_escape(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 8);
        for c in value.chars() {
            match c {
                '!' => out.push_str("\\x21"),
                '\\' => out.push_str("\\\\"),
                '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' => {
                    out.push('\\');
                    out.push(c);
                }
                '"' => out.push_str("\\\""),
                _ => out.push(c),
            }
        }
        out
    }

    fn group_clause(name: &str) -> String {
        if has_bang(name) {
            format!(r#"Group ~ "^{}$""#, regex_escape(name))
        } else {
            format!(r#"Group = "{}""#, escape_dsl_string(name))
        }
    }

    fn channel_keep_clause(group_name: &str, kept: &[String]) -> String {
        let any_bang = kept.iter().any(|n| has_bang(n));
        // `!=` also contains a literal `!` that the textual placeholder scanner
        // would pair with the next one, so negation is always spelled `NOT (...)`.
        if any_bang {
            let clauses =
                kept.iter().map(|n| format!(r#"Caption ~ "^{}$""#, regex_escape(n))).collect::<Vec<_>>().join(" OR ");
            format!(r"({} AND NOT ({}))", group_clause(group_name), clauses)
        } else if kept.len() == 1 {
            format!(r#"({} AND NOT (Caption = "{}"))"#, group_clause(group_name), escape_dsl_string(&kept[0]))
        } else {
            let values = kept.iter().map(|n| format!(r#""{}""#, escape_dsl_string(n))).collect::<Vec<_>>().join(", ");
            format!(r"({} AND NOT (Caption IN [{}]))", group_clause(group_name), values)
        }
    }

    let mut removals = Vec::new();

    for rule in rules {
        let cat_name = rule.name.trim();
        if cat_name.is_empty() {
            continue;
        }

        if !rule.is_enabled || rule.kept_channel_names.is_empty() {
            removals.push(group_clause(cat_name));
        } else {
            removals.push(channel_keep_clause(cat_name, &rule.kept_channel_names));
        }
    }

    if removals.is_empty() {
        return Ok(existing_filter.map(str::to_string));
    }

    let removal_combined =
        if removals.len() == 1 { format!("NOT ({})", removals[0]) } else { format!("NOT ({})", removals.join(" OR ")) };

    get_filter(&removal_combined, None).map_err(|err| FilterBuilderError::root(err.to_string()))?;

    let final_expression = match existing_filter.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) if extra.contains('!') => {
            format!("({extra}) AND {removal_combined}")
        }
        _ => removal_combined,
    };

    Ok(Some(final_expression))
}

fn managed_filter_rule_templates_unchecked(
    rules: &[ManagedFilterRule],
) -> Result<Vec<PatternTemplate>, FilterBuilderError> {
    rules
        .iter()
        .map(|rule| {
            let value = compile_filter_node(&rule.root, rules)?;
            Ok(PatternTemplate {
                name: rule.template_name(),
                value: TemplateValue::Single(value),
                filter_rule: Some(rule.into()),
                placeholder: rule.placeholder(),
            })
        })
        .collect()
}

fn compile_node(
    node: &FilterNode,
    rules: &[ManagedFilterRule],
    path: &mut Vec<usize>,
) -> Result<String, FilterBuilderError> {
    match node {
        FilterNode::Group { operator, children } => {
            if children.is_empty() {
                return Err(FilterBuilderError::at(path, "A filter group needs at least one child"));
            }
            let separator = match operator {
                FilterGroupOperator::All => " AND ",
                FilterGroupOperator::Any => " OR ",
            };
            let mut rendered = Vec::with_capacity(children.len());
            for (index, child) in children.iter().enumerate() {
                path.push(index);
                rendered.push(compile_node(child, rules, path)?);
                path.pop();
            }
            Ok(format!("({})", rendered.join(separator)))
        }
        FilterNode::Not { node } => Ok(format!("NOT ({})", compile_node(node, rules, path)?)),
        FilterNode::Regex { field, pattern } => {
            if matches!(field, ItemField::Type | ItemField::Chno | ItemField::Quality) {
                return Err(FilterBuilderError::at(path, format!("{field} does not support regular expressions")));
            }
            Ok(format!(r#"{field} ~ "{}""#, escape_filter_string(pattern)))
        }
        FilterNode::String { field, operator, value } => {
            if matches!(field, ItemField::Type | ItemField::Chno | ItemField::Quality) {
                return Err(FilterBuilderError::at(path, format!("{field} does not support text comparison")));
            }
            Ok(format!(r#"{field} {operator} "{}""#, escape_filter_string(value)))
        }
        FilterNode::Presence { field, operator } => {
            if matches!(field, ItemField::Type | ItemField::Chno | ItemField::Quality) {
                return Err(FilterBuilderError::at(path, format!("{field} does not support presence comparison")));
            }
            Ok(format!("{field} {operator}"))
        }
        FilterNode::Number { field, operator, value } => {
            if !matches!(field, ItemField::Chno | ItemField::Quality) {
                return Err(FilterBuilderError::at(path, format!("{field} is not numeric")));
            }
            Ok(format!("{field} {operator} {value}"))
        }
        FilterNode::Set { field, values } => {
            if matches!(field, ItemField::Type | ItemField::Chno | ItemField::Quality) {
                return Err(FilterBuilderError::at(path, format!("{field} does not support a value set")));
            }
            if values.is_empty() {
                return Err(FilterBuilderError::at(path, "A value set cannot be empty"));
            }
            let values = values
                .iter()
                .map(|value| format!(r#""{}""#, escape_filter_string(value)))
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!("{field} IN [{values}]"))
        }
        FilterNode::Type { value } => Ok(format!(
            "Type = {}",
            match value {
                FilterTypeValue::Live => "live",
                FilterTypeValue::Movie => "movie",
                FilterTypeValue::Series => "series",
            }
        )),
        FilterNode::Reference { rule_id } => rules
            .iter()
            .find(|rule| rule.id == *rule_id)
            .map(ManagedFilterRule::placeholder)
            .ok_or_else(|| FilterBuilderError::at(path, format!("Unknown shared filter rule '{rule_id}'"))),
        FilterNode::LegacyReference { template } => {
            let template = template.trim();
            if template.is_empty() || template.contains(['!', '"']) {
                return Err(FilterBuilderError::at(path, "Invalid template reference"));
            }
            Ok(format!("!{template}!"))
        }
        FilterNode::Raw { expression } => {
            let expression = expression.trim();
            if expression.is_empty() {
                return Err(FilterBuilderError::at(path, "An advanced expression cannot be empty"));
            }
            Ok(expression.to_string())
        }
    }
}

fn escape_filter_string(value: &str) -> String {
    value.replace('"', "\\\"")
}

fn exact_managed_reference(expression: &str, rules: &[ManagedFilterRule]) -> Option<String> {
    rules.iter().find(|rule| expression == rule.placeholder()).map(|rule| rule.id.clone())
}

const MANAGED_REFERENCE_SENTINEL_PREFIX: &str = "__TP_FILTER_RULE_REF_";
const LEGACY_REFERENCE_SENTINEL_PREFIX: &str = "__TP_TEMPLATE_REF_";

#[derive(Clone)]
enum PlaceholderTarget {
    Managed(String),
    Legacy(String),
}

fn replace_placeholder_references(
    expression: &str,
    rules: &[ManagedFilterRule],
    templates: Option<&[PatternTemplate]>,
) -> (String, HashMap<String, PlaceholderTarget>) {
    let mut result = expression.to_string();
    let mut sentinels = HashMap::new();
    for (index, rule) in rules.iter().enumerate() {
        let placeholder = rule.placeholder();
        if result.contains(&placeholder) {
            let sentinel = format!("{MANAGED_REFERENCE_SENTINEL_PREFIX}{index}__");
            result = result.replace(&placeholder, &format!(r#"Group = "{sentinel}""#));
            sentinels.insert(sentinel, PlaceholderTarget::Managed(rule.id.clone()));
        }
    }
    if let Some(templates) = templates {
        for (index, template) in templates.iter().enumerate() {
            if template.is_managed_filter_rule() {
                continue;
            }
            let placeholder = format!("!{}!", template.name);
            if result.contains(&placeholder) {
                let sentinel = format!("{LEGACY_REFERENCE_SENTINEL_PREFIX}{index}__");
                result = result.replace(&placeholder, &format!(r#"Group = "{sentinel}""#));
                sentinels.insert(sentinel, PlaceholderTarget::Legacy(template.name.clone()));
            }
        }
    }
    (result, sentinels)
}

fn restore_placeholder_references(node: &mut FilterNode, sentinels: &HashMap<String, PlaceholderTarget>) -> bool {
    match node {
        FilterNode::Group { children, .. } => {
            children.iter_mut().all(|child| restore_placeholder_references(child, sentinels))
        }
        FilterNode::Not { node } => restore_placeholder_references(node, sentinels),
        FilterNode::String { field: ItemField::Group, operator: StringOperator::Eq, value } => {
            if let Some(target) = sentinels.get(value) {
                *node = match target {
                    PlaceholderTarget::Managed(rule_id) => FilterNode::Reference { rule_id: rule_id.clone() },
                    PlaceholderTarget::Legacy(template) => FilterNode::LegacyReference { template: template.clone() },
                };
            }
            true
        }
        FilterNode::String { value, .. }
        | FilterNode::Regex { pattern: value, .. }
        | FilterNode::Raw { expression: value } => {
            !value.starts_with(MANAGED_REFERENCE_SENTINEL_PREFIX)
                && !value.starts_with(LEGACY_REFERENCE_SENTINEL_PREFIX)
        }
        FilterNode::Set { values, .. } => values.iter().all(|value| {
            !value.starts_with(MANAGED_REFERENCE_SENTINEL_PREFIX)
                && !value.starts_with(LEGACY_REFERENCE_SENTINEL_PREFIX)
        }),
        FilterNode::Presence { .. }
        | FilterNode::Number { .. }
        | FilterNode::Type { .. }
        | FilterNode::Reference { .. }
        | FilterNode::LegacyReference { .. } => true,
    }
}

fn contains_template_placeholder(expression: &str) -> bool {
    let bytes = expression.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'!' {
            if let Some(relative_end) = bytes[index + 1..].iter().position(|byte| *byte == b'!') {
                if relative_end > 0 {
                    return true;
                }
                index += relative_end + 2;
                continue;
            }
        }
        index += 1;
    }
    false
}

fn filter_to_node(filter: &Filter) -> FilterNode {
    match filter {
        Filter::Group(inner) => {
            FilterNode::Group { operator: FilterGroupOperator::All, children: vec![filter_to_node(inner)] }
        }
        Filter::FieldComparison(field, regex) => FilterNode::Regex { field: *field, pattern: regex.restr.clone() },
        Filter::TypeComparison(_, item_type) => FilterNode::Type {
            value: match item_type {
                PlaylistItemType::Live
                | PlaylistItemType::LiveHls
                | PlaylistItemType::LiveDash
                | PlaylistItemType::LiveUnknown => FilterTypeValue::Live,
                PlaylistItemType::Video | PlaylistItemType::LocalVideo | PlaylistItemType::Catchup => {
                    FilterTypeValue::Movie
                }
                _ => FilterTypeValue::Series,
            },
        },
        Filter::StringComparison(field, operator, value) => {
            FilterNode::String { field: *field, operator: *operator, value: value.clone() }
        }
        Filter::PresenceComparison(field, operator) => FilterNode::Presence { field: *field, operator: *operator },
        Filter::NumericComparison(field, operator, value) => {
            FilterNode::Number { field: *field, operator: *operator, value: *value }
        }
        Filter::SetComparison(field, values) => FilterNode::Set { field: *field, values: values.clone() },
        Filter::UnaryExpression(UnaryOperator::Not, inner) => FilterNode::Not { node: Box::new(filter_to_node(inner)) },
        Filter::BinaryExpression(left, operator, right) => binary_to_group(left, *operator, right),
    }
}

fn binary_to_group(left: &Filter, operator: BinaryOperator, right: &Filter) -> FilterNode {
    let group_operator = match operator {
        BinaryOperator::And => FilterGroupOperator::All,
        BinaryOperator::Or => FilterGroupOperator::Any,
    };
    let mut children = Vec::new();
    append_binary_child(&mut children, left, operator);
    append_binary_child(&mut children, right, operator);
    FilterNode::Group { operator: group_operator, children }
}

fn append_binary_child(children: &mut Vec<FilterNode>, filter: &Filter, parent_operator: BinaryOperator) {
    if let Filter::BinaryExpression(left, operator, right) = filter {
        if *operator == parent_operator {
            append_binary_child(children, left, parent_operator);
            append_binary_child(children, right, parent_operator);
            return;
        }
    }
    children.push(filter_to_node(filter));
}

pub fn validate_filter_library(
    rules: &[ManagedFilterRule],
    roots: &[(String, FilterNode)],
) -> Result<FilterLibraryValidation, FilterBuilderError> {
    let mut ids = HashSet::with_capacity(rules.len());
    let mut names = HashSet::with_capacity(rules.len());
    let mut template_names = HashSet::with_capacity(rules.len());
    for rule in rules {
        if rule.id.trim().is_empty() {
            return Err(FilterBuilderError::root("Shared filter rule id cannot be empty"));
        }
        if rule.name.trim().is_empty() {
            return Err(FilterBuilderError::root(format!("Shared filter rule '{}' needs a name", rule.id)));
        }
        if !ids.insert(rule.id.as_str()) {
            return Err(FilterBuilderError::root(format!("Duplicate shared filter rule id '{}'", rule.id)));
        }
        let normalized_name = rule.name.trim().to_ascii_lowercase();
        if !names.insert(normalized_name) {
            return Err(FilterBuilderError::root(format!("Duplicate shared filter rule name '{}'", rule.name)));
        }
        let template_name = managed_filter_rule_template_name(&rule.id);
        if !template_names.insert(template_name) {
            return Err(FilterBuilderError::root(format!(
                "Shared filter rule id '{}' collides after normalization",
                rule.id
            )));
        }
    }

    let by_id = rules.iter().map(|rule| (rule.id.as_str(), rule)).collect::<HashMap<_, _>>();
    let mut validation = FilterLibraryValidation::default();
    for rule in rules {
        collect_usages(&rule.root, &format!("rule:{}", rule.name), &by_id, &mut validation.usages, &mut Vec::new())?;
    }
    for (owner, root) in roots {
        collect_usages(root, owner, &by_id, &mut validation.usages, &mut Vec::new())?;
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for rule in rules {
        visit_rule(rule.id.as_str(), &by_id, &mut visiting, &mut visited)?;
    }

    for rule in rules {
        compile_filter_node(&rule.root, rules)?;
    }
    Ok(validation)
}

fn collect_usages(
    node: &FilterNode,
    owner: &str,
    rules: &HashMap<&str, &ManagedFilterRule>,
    usages: &mut HashMap<String, Vec<FilterRuleUsage>>,
    path: &mut Vec<usize>,
) -> Result<(), FilterBuilderError> {
    match node {
        FilterNode::Group { children, .. } => {
            for (index, child) in children.iter().enumerate() {
                path.push(index);
                collect_usages(child, owner, rules, usages, path)?;
                path.pop();
            }
        }
        FilterNode::Not { node } => {
            path.push(0);
            collect_usages(node, owner, rules, usages, path)?;
            path.pop();
        }
        FilterNode::Reference { rule_id } => {
            if !rules.contains_key(rule_id.as_str()) {
                return Err(FilterBuilderError::at(path, format!("Unknown shared filter rule '{rule_id}'")));
            }
            usages
                .entry(rule_id.clone())
                .or_default()
                .push(FilterRuleUsage { owner: owner.to_string(), path: path.clone() });
        }
        FilterNode::Regex { .. }
        | FilterNode::String { .. }
        | FilterNode::Presence { .. }
        | FilterNode::Number { .. }
        | FilterNode::Set { .. }
        | FilterNode::Type { .. }
        | FilterNode::LegacyReference { .. }
        | FilterNode::Raw { .. } => {}
    }
    Ok(())
}

fn visit_rule<'a>(
    rule_id: &'a str,
    rules: &HashMap<&'a str, &'a ManagedFilterRule>,
    visiting: &mut HashSet<&'a str>,
    visited: &mut HashSet<&'a str>,
) -> Result<(), FilterBuilderError> {
    if visited.contains(rule_id) {
        return Ok(());
    }
    if !visiting.insert(rule_id) {
        return Err(FilterBuilderError::root(format!("Cyclic shared filter rule dependency at '{rule_id}'")));
    }
    let rule = rules
        .get(rule_id)
        .ok_or_else(|| FilterBuilderError::root(format!("Unknown shared filter rule '{rule_id}'")))?;
    let mut dependencies = Vec::new();
    rule.root.visit_references(&mut |dependency| dependencies.push(dependency.to_string()));
    for dependency in dependencies {
        let dependency_rule = rules
            .get(dependency.as_str())
            .ok_or_else(|| FilterBuilderError::root(format!("Unknown shared filter rule '{dependency}'")))?;
        visit_rule(dependency_rule.id.as_str(), rules, visiting, visited)?;
    }
    visiting.remove(rule_id);
    visited.insert(rule_id);
    Ok(())
}

pub fn prepare_managed_filter_rule_templates(
    rules: &[ManagedFilterRule],
) -> Result<Vec<PatternTemplate>, TuliproxError> {
    let mut templates =
        managed_filter_rule_templates(rules).map_err(|error| TuliproxError::FilterParse(error.to_string()))?;
    super::prepare_templates(&mut templates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundation::NumericOperator;

    fn rule(id: &str, name: &str, root: FilterNode) -> ManagedFilterRule {
        ManagedFilterRule { id: id.to_string(), name: name.to_string(), root }
    }

    fn legacy_template(name: &str, value: &str) -> PatternTemplate {
        PatternTemplate {
            name: name.to_string(),
            value: TemplateValue::Single(value.to_string()),
            filter_rule: None,
            placeholder: String::new(),
        }
    }

    #[test]
    fn valid_dsl_imports_and_compiles_semantically() {
        let original = r#"(Group = "Sports" OR Name CONTAINS "HD") AND NOT Chno < 10"#;
        let draft = import_filter_draft(Some(original), &[]).expect("draft");
        let compiled = compile_filter_draft(Some(&draft), &[]).expect("compile").expect("expression");
        assert!(get_filter(&compiled, None).is_ok());
        assert!(compiled.contains("AND"));
        assert!(compiled.contains("NOT"));
    }

    #[test]
    fn invalid_dsl_is_preserved_as_raw() {
        let original = "Group = (broken";
        assert_eq!(
            import_filter_draft(Some(original), &[]),
            Some(FilterNode::Raw { expression: original.to_string() })
        );
    }

    #[test]
    fn legacy_placeholder_is_preserved_as_raw() {
        let original = "!LEGACY! AND Type = live";
        assert_eq!(
            import_filter_draft(Some(original), &[]),
            Some(FilterNode::Raw { expression: original.to_string() })
        );
    }

    #[test]
    fn raw_legacy_filter_compiles_with_the_existing_template_catalog() {
        let original = "!NO_24H! AND !NO_ADULT! AND !NO_H265!";
        let draft = import_filter_draft(Some(original), &[]).expect("legacy filter draft");
        let templates = vec![
            legacy_template("NO_24H", r#"NOT Title ~ ".*24H.*""#),
            legacy_template("NO_ADULT", r#"NOT Title ~ ".*Adult.*""#),
            legacy_template("NO_H265", r#"NOT Title ~ ".*H265.*""#),
        ];

        assert_eq!(
            compile_filter_draft_with_templates(Some(&draft), &[], Some(&templates))
                .expect("legacy templates should validate")
                .as_deref(),
            Some(original)
        );
    }

    #[test]
    fn legacy_templates_import_as_visual_references() {
        let original = "!NO_24H! AND !NO_ADULT! AND !NO_H265!";
        let templates = vec![
            legacy_template("NO_24H", r#"NOT Title ~ ".*24H.*""#),
            legacy_template("NO_ADULT", r#"NOT Title ~ ".*Adult.*""#),
            legacy_template("NO_H265", r#"NOT Title ~ ".*H265.*""#),
        ];

        let draft =
            import_filter_draft_with_templates(Some(original), &[], Some(&templates)).expect("visual legacy draft");
        let FilterNode::Group { operator, children } = &draft else {
            panic!("legacy placeholders should import as a visual group, got {draft:?}")
        };
        assert_eq!(*operator, FilterGroupOperator::All);
        assert_eq!(children.len(), 3);
        for (child, expected) in children.iter().zip(["NO_24H", "NO_ADULT", "NO_H265"]) {
            assert!(matches!(child, FilterNode::LegacyReference { template } if template == expected));
        }

        assert_eq!(
            compile_filter_draft_with_templates(Some(&draft), &[], Some(&templates))
                .expect("visual legacy tree should compile")
                .as_deref(),
            Some(r"(!NO_24H! AND !NO_ADULT! AND !NO_H265!)")
        );
    }

    #[test]
    fn unknown_placeholder_still_falls_back_to_raw() {
        let templates = vec![legacy_template("KNOWN", "Type = live")];
        assert_eq!(
            import_filter_draft_with_templates(Some("!KNOWN! AND !UNKNOWN!"), &[], Some(&templates)),
            Some(FilterNode::Raw { expression: "!KNOWN! AND !UNKNOWN!".to_string() })
        );
    }

    #[test]
    fn legacy_reference_inside_regex_falls_back_to_raw() {
        let templates = vec![legacy_template("DELIM", r"[\s_-]*")];
        assert_eq!(
            import_filter_draft_with_templates(Some(r#"Group ~ "^DE!DELIM!.*""#), &[], Some(&templates)),
            Some(FilterNode::Raw { expression: r#"Group ~ "^DE!DELIM!.*""#.to_string() })
        );
    }

    #[test]
    fn exact_managed_placeholder_imports_as_reference() {
        let rules = vec![rule("abc", "Sports", FilterNode::Type { value: FilterTypeValue::Live })];
        let draft = import_filter_draft(Some("!TP_FILTER_RULE_ABC!"), &rules).expect("draft");
        assert_eq!(draft, FilterNode::Reference { rule_id: "abc".to_string() });
        assert_eq!(compile_filter_draft(Some(&draft), &rules).unwrap().as_deref(), Some("!TP_FILTER_RULE_ABC!"));
    }

    #[test]
    fn managed_placeholder_inside_expression_imports_as_reference_node() {
        let rules = vec![rule("abc", "Sports", FilterNode::Type { value: FilterTypeValue::Live })];
        let draft = import_filter_draft(Some("!TP_FILTER_RULE_ABC! AND Chno > 10"), &rules).expect("draft");
        let FilterNode::Group { children, .. } = draft else { panic!("expected group") };
        assert!(matches!(children.first(), Some(FilterNode::Reference { rule_id }) if rule_id == "abc"));
    }

    #[test]
    fn compiler_escapes_quotes_and_validates_field_kinds() {
        let draft = FilterNode::String {
            field: ItemField::Name,
            operator: StringOperator::Eq,
            value: "A \"quoted\" name".to_string(),
        };
        assert_eq!(compile_filter_draft(Some(&draft), &[]).unwrap().as_deref(), Some(r#"Name = "A \"quoted\" name""#));
        let invalid = FilterNode::Number { field: ItemField::Name, operator: NumericOperator::Eq, value: 1 };
        assert!(compile_filter_draft(Some(&invalid), &[]).is_err());
    }

    #[test]
    fn library_reports_usages_and_rejects_cycles() {
        let leaf = rule("leaf", "Leaf", FilterNode::Type { value: FilterTypeValue::Live });
        let parent = rule("parent", "Parent", FilterNode::Reference { rule_id: "leaf".to_string() });
        let root = FilterNode::Reference { rule_id: "parent".to_string() };
        let validation =
            validate_filter_library(&[leaf.clone(), parent.clone()], &[("target:one:processing".to_string(), root)])
                .expect("valid library");
        assert_eq!(validation.usages["leaf"][0].owner, "rule:Parent");
        assert_eq!(validation.usages["parent"][0].owner, "target:one:processing");

        let cyclic = vec![
            rule("one", "One", FilterNode::Reference { rule_id: "two".to_string() }),
            rule("two", "Two", FilterNode::Reference { rule_id: "one".to_string() }),
        ];
        assert!(validate_filter_library(&cyclic, &[]).is_err());
    }

    #[test]
    fn managed_templates_keep_metadata_and_runtime_value() {
        let rules = vec![rule(
            "sports",
            "Sports only",
            FilterNode::String { field: ItemField::Group, operator: StringOperator::Eq, value: "Sports".to_string() },
        )];
        let templates = managed_filter_rule_templates(&rules).expect("templates");
        assert_eq!(templates[0].name, "TP_FILTER_RULE_SPORTS");
        assert_eq!(templates[0].value, TemplateValue::Single(r#"Group = "Sports""#.to_string()));
        assert_eq!(managed_filter_rules(Some(&templates)), rules);
    }

    #[test]
    fn compile_visual_rules_produces_valid_precise_dsl() {
        let rules = vec![
            VisualCategoryRule { name: "Adultos".to_string(), is_enabled: false, kept_channel_names: Vec::new() },
            VisualCategoryRule {
                name: "Documentarios".to_string(),
                is_enabled: true,
                kept_channel_names: vec!["Discovery".to_string(), "NatGeo".to_string()],
            },
            VisualCategoryRule {
                name: "Noticias".to_string(),
                is_enabled: true,
                kept_channel_names: vec!["CNN".to_string()],
            },
        ];

        let dsl = compile_visual_rules_to_filter_dsl(None, &rules).unwrap().unwrap();
        assert!(dsl.starts_with("NOT ("));
        assert!(dsl.contains(r#"Group = "Adultos""#));
        assert!(dsl.contains(r#"Group = "Documentarios" AND NOT (Caption IN ["Discovery", "NatGeo"])"#));
        assert!(dsl.contains(r#"(Group = "Noticias" AND NOT (Caption = "CNN"))"#));
        // The generated DSL must never contain a literal `!` (template placeholder marker).
        assert!(!dsl.replace("\\x21", "").contains('!'));

        // Must parse cleanly in Pest
        let filter = get_filter(&dsl, None).expect("DSL must parse cleanly");
        assert!(format!("{filter:?}").contains("Group"));
    }

    #[test]
    fn compile_visual_rules_preserves_existing_templates() {
        let rules =
            vec![VisualCategoryRule { name: "Movies".to_string(), is_enabled: false, kept_channel_names: Vec::new() }];

        let dsl = compile_visual_rules_to_filter_dsl(Some("!NO_24H!"), &rules).unwrap().unwrap();
        assert_eq!(dsl, r#"(!NO_24H!) AND NOT (Group = "Movies")"#);
    }
}

#[cfg(test)]
mod visual_dsl_tests {
    use super::*;
    use crate::foundation::get_filter;

    #[test]
    fn reproduces_live_category_names_parse() {
        let names = [
            "Canais | Argentina",
            "Canais ⫸ ESPN & TNT Sports⚽",
            "Canais ⫸ Max & Disney+⚽",
            "⚽ Guia de Jogos",
            "🌾 A Fazenda 18",
            "💪 Mr. Olympia 2026",
            "🛡️ Área do Cliente",
            "Canais ⫸ Prime VIDEO e DAZN⚽",
        ];
        let rules: Vec<VisualCategoryRule> = names
            .iter()
            .map(|n| VisualCategoryRule { name: n.to_string(), is_enabled: false, kept_channel_names: vec![] })
            .collect();
        let dsl = compile_visual_rules_to_filter_dsl(None, &rules).unwrap().unwrap();
        get_filter(&dsl, None).unwrap_or_else(|e| panic!("parse failed for DSL: {dsl}\nerror: {e}"));
    }

    #[test]
    fn names_with_bang_are_regex_encoded_and_parse() {
        let rules = vec![
            VisualCategoryRule {
                name: "Canais | Especiais!".to_string(),
                is_enabled: false,
                kept_channel_names: vec![],
            },
            VisualCategoryRule {
                name: "US Shows".to_string(),
                is_enabled: true,
                kept_channel_names: vec![
                    "US: E!, ! Entertainment HD".to_string(),
                    "US: Atlanta Channel HD".to_string(),
                ],
            },
        ];
        let dsl = compile_visual_rules_to_filter_dsl(Some("!NO_24H!"), &rules).unwrap().unwrap();
        assert!(dsl.contains(r#"Group ~ "^Canais \| Especiais\x21$""#));
        assert!(dsl.contains(r#"Caption ~ "^US: E\x21, \x21 Entertainment HD$""#));
        // The only remaining `!` pair is the intentional !NO_24H! placeholder.
        let without_hex = dsl.replace("\\x21", "");
        assert_eq!(without_hex.matches('!').count(), 2);
        let mut templates = vec![crate::model::PatternTemplate {
            name: "NO_24H".to_string(),
            value: crate::model::TemplateValue::Single(r#"Group ~ ".*""#.to_string()),
            filter_rule: None,
            placeholder: String::new(),
        }];
        crate::foundation::prepare_templates(&mut templates).unwrap();
        get_filter(&dsl, Some(&templates)).unwrap_or_else(|e| panic!("parse failed for DSL: {dsl}\nerror: {e}"));
    }

    #[test]
    fn reproduces_caption_in_parse() {
        let rules = vec![VisualCategoryRule {
            name: "Canais ⫸ Desenhos 24h".to_string(),
            is_enabled: true,
            kept_channel_names: vec!["Kids A".to_string(), "Kids B".to_string()],
        }];
        let dsl = compile_visual_rules_to_filter_dsl(None, &rules).unwrap().unwrap();
        match get_filter(&dsl, None) {
            Ok(_) => {}
            Err(e) => panic!("parse failed for DSL: {dsl}\nerror: {e}"),
        }
    }
}
