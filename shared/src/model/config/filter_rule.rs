use crate::{
    foundation::{NumericOperator, PresenceOperator, StringOperator},
    model::ItemField,
};

pub const FILTER_RULE_SCHEMA_VERSION: u16 = 1;
pub const MANAGED_FILTER_RULE_TEMPLATE_PREFIX: &str = "TP_FILTER_RULE_";

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilterGroupOperator {
    #[default]
    All,
    Any,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilterTypeValue {
    Live,
    Movie,
    Series,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FilterNode {
    Group {
        #[serde(default)]
        operator: FilterGroupOperator,
        #[serde(default)]
        children: Vec<FilterNode>,
    },
    Not {
        node: Box<FilterNode>,
    },
    Regex {
        field: ItemField,
        pattern: String,
    },
    String {
        field: ItemField,
        operator: StringOperator,
        value: String,
    },
    Presence {
        field: ItemField,
        operator: PresenceOperator,
    },
    Number {
        field: ItemField,
        operator: NumericOperator,
        value: u32,
    },
    Set {
        field: ItemField,
        values: Vec<String>,
    },
    Type {
        value: FilterTypeValue,
    },
    Reference {
        rule_id: String,
    },
    LegacyReference {
        template: String,
    },
    Raw {
        expression: String,
    },
}

impl FilterNode {
    pub fn all(children: Vec<Self>) -> Self {
        Self::Group { operator: FilterGroupOperator::All, children }
    }

    pub fn any(children: Vec<Self>) -> Self {
        Self::Group { operator: FilterGroupOperator::Any, children }
    }

    pub fn visit_references(&self, visitor: &mut impl FnMut(&str)) {
        match self {
            Self::Group { children, .. } => {
                for child in children {
                    child.visit_references(visitor);
                }
            }
            Self::Not { node } => node.visit_references(visitor),
            Self::Reference { rule_id } => visitor(rule_id),
            Self::Regex { .. }
            | Self::String { .. }
            | Self::Presence { .. }
            | Self::Number { .. }
            | Self::Set { .. }
            | Self::Type { .. }
            | Self::LegacyReference { .. }
            | Self::Raw { .. } => {}
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedFilterRule {
    pub id: String,
    pub name: String,
    pub root: FilterNode,
}

impl ManagedFilterRule {
    pub fn template_name(&self) -> String {
        managed_filter_rule_template_name(&self.id)
    }

    pub fn placeholder(&self) -> String {
        format!("!{}!", self.template_name())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedFilterRuleMetadata {
    #[serde(default = "default_filter_rule_schema_version")]
    pub version: u16,
    pub id: String,
    pub name: String,
    pub root: FilterNode,
}

impl From<&ManagedFilterRule> for ManagedFilterRuleMetadata {
    fn from(rule: &ManagedFilterRule) -> Self {
        Self {
            version: FILTER_RULE_SCHEMA_VERSION,
            id: rule.id.clone(),
            name: rule.name.clone(),
            root: rule.root.clone(),
        }
    }
}

impl From<&ManagedFilterRuleMetadata> for ManagedFilterRule {
    fn from(metadata: &ManagedFilterRuleMetadata) -> Self {
        Self { id: metadata.id.clone(), name: metadata.name.clone(), root: metadata.root.clone() }
    }
}

const fn default_filter_rule_schema_version() -> u16 {
    FILTER_RULE_SCHEMA_VERSION
}

pub fn managed_filter_rule_template_name(id: &str) -> String {
    let mut result = String::with_capacity(MANAGED_FILTER_RULE_TEMPLATE_PREFIX.len() + id.len());
    result.push_str(MANAGED_FILTER_RULE_TEMPLATE_PREFIX);
    for character in id.chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character.to_ascii_uppercase());
        } else {
            result.push('_');
        }
    }
    result
}

pub fn new_managed_filter_rule_id() -> String {
    format!("{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourcesEditorSaveDto {
    pub sources: super::SourcesConfigDto,
    /// `None` means that this client does not manage the visual rule catalog.
    /// `Some([])` explicitly removes every managed rule while preserving legacy templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_rules: Option<Vec<ManagedFilterRule>>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum SourcesEditorSaveRepr {
    Editor {
        sources: super::SourcesConfigDto,
        #[serde(default)]
        filter_rules: Option<Vec<ManagedFilterRule>>,
    },
    Legacy(super::SourcesConfigDto),
}

impl<'de> serde::Deserialize<'de> for SourcesEditorSaveDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match SourcesEditorSaveRepr::deserialize(deserializer)? {
            SourcesEditorSaveRepr::Editor { sources, filter_rules } => Self { sources, filter_rules },
            SourcesEditorSaveRepr::Legacy(sources) => Self { sources, filter_rules: None },
        })
    }
}

impl From<super::SourcesConfigDto> for SourcesEditorSaveDto {
    fn from(sources: super::SourcesConfigDto) -> Self {
        Self { sources, filter_rules: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_template_names_are_stable_and_safe() {
        assert_eq!(managed_filter_rule_template_name("ab-c 12"), "TP_FILTER_RULE_AB_C_12");
    }

    #[test]
    fn node_reference_visitor_walks_nested_nodes() {
        let node = FilterNode::all(vec![
            FilterNode::Reference { rule_id: "one".to_string() },
            FilterNode::Not {
                node: Box::new(FilterNode::any(vec![FilterNode::Reference { rule_id: "two".to_string() }])),
            },
        ]);
        let mut references = Vec::new();
        node.visit_references(&mut |id| references.push(id.to_string()));
        assert_eq!(references, vec!["one", "two"]);
    }
}
