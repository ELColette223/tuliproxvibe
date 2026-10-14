use super::filter_builder_model::{default_condition, default_group, NodePath};
use crate::{
    app::components::{input::Input, Card, DropDownOption, DropDownSelection, Select, TextArea, TextButton},
    i18n::use_translation,
};
use shared::{
    foundation::{NumericOperator, PresenceOperator, StringOperator},
    model::{FilterGroupOperator, FilterNode, FilterTypeValue, ItemField, ManagedFilterRule},
};
use std::{rc::Rc, str::FromStr};
use web_sys::InputEvent;
use yew::{prelude::*, TargetCast};

#[derive(Properties, Clone, PartialEq)]
pub struct FilterRuleTreeEditorProps {
    pub rule: ManagedFilterRule,
    pub all_rules: Vec<ManagedFilterRule>,
    pub on_change: Callback<FilterNode>,
    #[prop_or_default]
    pub readonly: bool,
}

#[component]
pub fn FilterRuleTreeEditor(props: &FilterRuleTreeEditorProps) -> Html {
    let translate = use_translation();
    let root = props.rule.root.clone();
    let root_for_update = root.clone();
    let on_replace = props.on_change.clone();
    html! {
        <div class="tp__filter-builder__rule-tree">
            { render_rule_node(&root, &Vec::new(), &props.all_rules, props.readonly, &translate, &Callback::from(move |(path, replacement): (NodePath, FilterNode)| {
                let mut next = root_for_update.clone();
                if let Some(target) = node_mut(&mut next, &path) {
                    *target = replacement;
                    on_replace.emit(next);
                }
            })) }
        </div>
    }
}

#[allow(clippy::too_many_lines)]
fn render_rule_node(
    node: &FilterNode,
    path: &NodePath,
    rules: &[ManagedFilterRule],
    readonly: bool,
    translate: &crate::i18n::YewI18n,
    replace: &Callback<(NodePath, FilterNode)>,
) -> Html {
    match node {
        FilterNode::Group { operator, children } => {
            let on_operator = {
                let path = path.clone();
                let children = children.clone();
                let replace = replace.clone();
                Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    let operator = if selection_value(&selection) == Some("any") {
                        FilterGroupOperator::Any
                    } else {
                        FilterGroupOperator::All
                    };
                    replace.emit((path.clone(), FilterNode::Group { operator, children: children.clone() }));
                })
            };
            let options = Rc::new(vec![
                DropDownOption::new(
                    "all",
                    html! { translate.t("FILTER_BUILDER.ALL") },
                    *operator == FilterGroupOperator::All,
                ),
                DropDownOption::new(
                    "any",
                    html! { translate.t("FILTER_BUILDER.ANY") },
                    *operator == FilterGroupOperator::Any,
                ),
            ]);
            html! {
                <Card class="tp__filter-builder__node tp__filter-builder__group">
                    <Select name={format!("rule-group-{}", key(path))} options={options} multi_select={false} on_select={on_operator} />
                    { for children.iter().enumerate().map(|(index, child)| {
                        let mut child_path = path.clone();
                        child_path.push(index);
                        render_rule_node(child, &child_path, rules, readonly, translate, replace)
                    }) }
                    if !readonly {
                        <div class="tp__filter-builder__add-row">
                            { append_button(path, node, default_condition(), translate.t("FILTER_BUILDER.ADD_CONDITION"), replace) }
                            { append_button(path, node, default_group(), translate.t("FILTER_BUILDER.ADD_GROUP"), replace) }
                            { append_button(path, node, FilterNode::Raw { expression: String::new() }, translate.t("FILTER_BUILDER.ADD_ADVANCED"), replace) }
                        </div>
                    }
                </Card>
            }
        }
        FilterNode::Not { node } => {
            let mut child_path = path.clone();
            child_path.push(0);
            html! {
                <Card class="tp__filter-builder__node tp__filter-builder__not">
                    <strong>{"NOT"}</strong>
                    {render_rule_node(node, &child_path, rules, readonly, translate, replace)}
                </Card>
            }
        }
        FilterNode::Reference { rule_id } => {
            let options = Rc::new(
                rules
                    .iter()
                    .map(|rule| DropDownOption::new(&rule.id, html! {rule.name.clone()}, rule.id == *rule_id))
                    .collect(),
            );
            let path = path.clone();
            let replace = replace.clone();
            html! {
                <Card class="tp__filter-builder__node">
                    <Select name={format!("rule-ref-{}", key(&path))} options={options} multi_select={false} on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                        if let Some(id) = selection_value(&selection) {
                            replace.emit((path.clone(), FilterNode::Reference { rule_id: id.to_string() }));
                        }
                    })} />
                </Card>
            }
        }
        FilterNode::Raw { expression } => {
            let path = path.clone();
            let replace = replace.clone();
            html! {
                <Card class="tp__filter-builder__node tp__filter-builder__raw">
                    <TextArea name={format!("rule-raw-{}", key(&path))} value={expression.clone()} rows={Some(4)} on_change={Some(Callback::from(move |expression| {
                        replace.emit((path.clone(), FilterNode::Raw { expression }));
                    }))} />
                </Card>
            }
        }
        FilterNode::LegacyReference { template } => {
            html! {
                <Card class="tp__filter-builder__node">
                    <strong>{ format!("{}: !{}!", translate.t("FILTER_BUILDER.TEMPLATE"), template) }</strong>
                </Card>
            }
        }
        _ => render_leaf(node, path, readonly, translate, replace),
    }
}

fn render_leaf(
    node: &FilterNode,
    path: &NodePath,
    readonly: bool,
    translate: &crate::i18n::YewI18n,
    replace: &Callback<(NodePath, FilterNode)>,
) -> Html {
    let field = field_of(node);
    let fields = [
        ItemField::Group,
        ItemField::Name,
        ItemField::Title,
        ItemField::Caption,
        ItemField::Genre,
        ItemField::Url,
        ItemField::Input,
        ItemField::EpgId,
        ItemField::Type,
        ItemField::Chno,
        ItemField::Quality,
    ];
    let field_options = Rc::new(
        fields
            .into_iter()
            .map(|candidate| DropDownOption::new(candidate.as_ref(), html! {candidate.to_string()}, candidate == field))
            .collect(),
    );
    let on_field = {
        let path = path.clone();
        let replace = replace.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(field) = selection_value(&selection).and_then(|value| ItemField::from_str(value).ok()) {
                replace.emit((path.clone(), super::filter_builder_model::default_node_for_field(field)));
            }
        })
    };
    html! {
        <Card class="tp__filter-builder__node tp__filter-builder__condition">
            <Select name={format!("rule-field-{}", key(path))} options={field_options} multi_select={false} on_select={on_field} />
            { leaf_value(node, path, replace) }
            if !readonly {
                { leaf_actions(path, node, translate.t("FILTER_BUILDER.TOGGLE_NOT"), replace) }
            }
        </Card>
    }
}

fn leaf_value(node: &FilterNode, path: &NodePath, replace: &Callback<(NodePath, FilterNode)>) -> Html {
    match node {
        FilterNode::String { field, operator, value } => {
            let operators = [
                ("eq", StringOperator::Eq),
                ("not_eq", StringOperator::NotEq),
                ("contains", StringOperator::Contains),
                ("starts_with", StringOperator::StartsWith),
            ];
            let options = Rc::new(
                operators
                    .iter()
                    .map(|(id, candidate)| {
                        DropDownOption::new(id, html! {candidate.to_string()}, *candidate == *operator)
                    })
                    .collect(),
            );
            let on_operator = {
                let path = path.clone();
                let replace = replace.clone();
                let field = *field;
                let value = value.clone();
                Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    if let Some((_, operator)) =
                        operators.iter().find(|(id, _)| Some(*id) == selection_value(&selection))
                    {
                        replace.emit((
                            path.clone(),
                            FilterNode::String { field, operator: *operator, value: value.clone() },
                        ));
                    }
                })
            };
            let on_value = {
                let path = path.clone();
                let replace = replace.clone();
                let field = *field;
                let operator = *operator;
                Callback::from(move |value| replace.emit((path.clone(), FilterNode::String { field, operator, value })))
            };
            html! { <><Select name={format!("rule-op-{}", key(path))} options={options} multi_select={false} on_select={on_operator} /><Input name={format!("rule-value-{}", key(path))} value={value.clone()} on_change={Some(on_value)} /></> }
        }
        FilterNode::Regex { field, pattern } => {
            let path = path.clone();
            let replace = replace.clone();
            let field = *field;
            html! { <><span>{"~"}</span><Input name={format!("rule-regex-{}", key(&path))} value={pattern.clone()} on_change={Some(Callback::from(move |pattern| replace.emit((path.clone(), FilterNode::Regex { field, pattern }))))} /></> }
        }
        FilterNode::Presence { field, operator } => {
            let options = Rc::new(vec![
                DropDownOption::new("empty", html! {"IS EMPTY"}, *operator == PresenceOperator::IsEmpty),
                DropDownOption::new("not_empty", html! {"IS NOT EMPTY"}, *operator == PresenceOperator::IsNotEmpty),
            ]);
            let path = path.clone();
            let replace = replace.clone();
            let field = *field;
            html! { <Select name={format!("rule-presence-{}", key(&path))} options={options} multi_select={false} on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                let operator = if selection_value(&selection) == Some("not_empty") { PresenceOperator::IsNotEmpty } else { PresenceOperator::IsEmpty };
                replace.emit((path.clone(), FilterNode::Presence { field, operator }));
            })} /> }
        }
        FilterNode::Number { field, operator, value } => {
            let operators = [
                ("eq", NumericOperator::Eq),
                ("not_eq", NumericOperator::NotEq),
                ("gt", NumericOperator::Greater),
                ("gte", NumericOperator::GreaterOrEqual),
                ("lt", NumericOperator::Less),
                ("lte", NumericOperator::LessOrEqual),
            ];
            let options = Rc::new(
                operators
                    .iter()
                    .map(|(id, candidate)| {
                        DropDownOption::new(id, html! {candidate.to_string()}, *candidate == *operator)
                    })
                    .collect(),
            );
            let path_for_op = path.clone();
            let replace_for_op = replace.clone();
            let field = *field;
            let value = *value;
            let on_operator = Callback::from(move |(_, selection): (String, DropDownSelection)| {
                if let Some((_, operator)) = operators.iter().find(|(id, _)| Some(*id) == selection_value(&selection)) {
                    replace_for_op
                        .emit((path_for_op.clone(), FilterNode::Number { field, operator: *operator, value }));
                }
            });
            let path_for_value = path.clone();
            let replace_for_value = replace.clone();
            let operator = *operator;
            let on_value = Callback::from(move |event: InputEvent| {
                if let Some(input) = event.target_dyn_into::<web_sys::HtmlInputElement>() {
                    if let Ok(value) = input.value().parse() {
                        replace_for_value.emit((path_for_value.clone(), FilterNode::Number { field, operator, value }));
                    }
                }
            });
            html! { <><Select name={format!("rule-num-op-{}", key(path))} options={options} multi_select={false} on_select={on_operator} /><input type="number" min="0" value={value.to_string()} oninput={on_value}/></> }
        }
        FilterNode::Set { field, values } => {
            let path = path.clone();
            let replace = replace.clone();
            let field = *field;
            html! { <><span>{"IN"}</span><Input name={format!("rule-set-{}", key(&path))} value={values.join(", ")} on_change={Some(Callback::from(move |value: String| {
                let values = value.split(',').map(str::trim).filter(|value| !value.is_empty()).map(str::to_string).collect();
                replace.emit((path.clone(), FilterNode::Set { field, values }));
            }))}/></> }
        }
        FilterNode::Type { value } => {
            let options = Rc::new(vec![
                DropDownOption::new("live", html! {"live"}, *value == FilterTypeValue::Live),
                DropDownOption::new("movie", html! {"movie"}, *value == FilterTypeValue::Movie),
                DropDownOption::new("series", html! {"series"}, *value == FilterTypeValue::Series),
            ]);
            let path = path.clone();
            let replace = replace.clone();
            html! { <Select name={format!("rule-type-{}", key(&path))} options={options} multi_select={false} on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                let value = match selection_value(&selection) { Some("movie") => FilterTypeValue::Movie, Some("series") => FilterTypeValue::Series, _ => FilterTypeValue::Live };
                replace.emit((path.clone(), FilterNode::Type { value }));
            })} /> }
        }
        _ => Html::default(),
    }
}

fn leaf_actions(path: &NodePath, node: &FilterNode, title: String, replace: &Callback<(NodePath, FilterNode)>) -> Html {
    let wrap = {
        let path = path.clone();
        let replace = replace.clone();
        let node = node.clone();
        Callback::from(move |_| replace.emit((path.clone(), FilterNode::Not { node: Box::new(node.clone()) })))
    };
    html! { <TextButton name="rule-not" icon="Clear" title={title} onclick={wrap}/> }
}

fn append_button(
    path: &NodePath,
    node: &FilterNode,
    child: FilterNode,
    title: String,
    replace: &Callback<(NodePath, FilterNode)>,
) -> Html {
    let FilterNode::Group { operator, children } = node else { return Html::default() };
    let path = path.clone();
    let replace = replace.clone();
    let operator = *operator;
    let mut next = children.clone();
    next.push(child);
    html! { <TextButton name="rule-add" icon="Add" title={title} onclick={Callback::from(move |_| replace.emit((path.clone(), FilterNode::Group { operator, children: next.clone() })))}/> }
}

fn field_of(node: &FilterNode) -> ItemField {
    match node {
        FilterNode::Regex { field, .. }
        | FilterNode::String { field, .. }
        | FilterNode::Presence { field, .. }
        | FilterNode::Number { field, .. }
        | FilterNode::Set { field, .. } => *field,
        FilterNode::Type { .. } => ItemField::Type,
        _ => ItemField::Group,
    }
}

fn node_mut<'a>(node: &'a mut FilterNode, path: &[usize]) -> Option<&'a mut FilterNode> {
    let Some((&head, tail)) = path.split_first() else { return Some(node) };
    match node {
        FilterNode::Group { children, .. } => children.get_mut(head).and_then(|child| node_mut(child, tail)),
        FilterNode::Not { node } if head == 0 => node_mut(node, tail),
        _ => None,
    }
}

fn selection_value(selection: &DropDownSelection) -> Option<&str> {
    match selection {
        DropDownSelection::Single(value) => Some(value),
        DropDownSelection::Multi(values) => values.first().map(String::as_str),
        DropDownSelection::Empty => None,
    }
}

fn key(path: &[usize]) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.iter().map(usize::to_string).collect::<Vec<_>>().join("-")
    }
}
