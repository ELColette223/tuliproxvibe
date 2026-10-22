use crate::model::{ManagedFilterRule, ManagedFilterRuleMetadata};
use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TemplateValue {
    Single(String),
    Multi(Vec<String>),
}

impl Display for TemplateValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateValue::Single(value) => write!(f, "{value}")?,
            TemplateValue::Multi(values) => {
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{value}")?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PatternTemplate {
    pub name: String,
    pub value: TemplateValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_rule: Option<ManagedFilterRuleMetadata>,
    #[serde(skip)]
    pub placeholder: String,
}

impl PatternTemplate {
    pub fn prepare(&mut self) {
        let mut placeholder = String::with_capacity(self.name.len() + 2);
        placeholder.push('!');
        placeholder.push_str(&self.name);
        placeholder.push('!');

        self.placeholder = placeholder;
    }

    pub const fn is_managed_filter_rule(&self) -> bool {
        self.filter_rule.is_some()
    }

    pub fn managed_filter_rule(&self) -> Option<ManagedFilterRule> {
        self.filter_rule.as_ref().map(ManagedFilterRule::from)
    }
}
