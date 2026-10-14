use super::filter_builder_model::{
    default_condition, default_group, default_node_for_field, parse_string_operator, reduce_filter_builder,
    string_operator_id, FilterBuilderAction, FilterStage, NodePath, TargetFilterBuilderState,
};
use crate::{
    app::{
        components::{
            build_options, input::Input, AppIcon, Card, ContentDialog, DropDownOption, DropDownSelection, FilterRuleTreeEditor,
            FilterView, Select, TabItem, TabSet, TextArea, TextButton,
        },
        ConfigContext,
    },
    i18n::use_translation,
    model::{DialogAction, DialogActions, DialogResult},
};
use shared::{
    foundation::{compile_filter_draft_with_templates, NumericOperator, PresenceOperator, StringOperator},
    model::{
        ConfigTargetFilterDto, FilterGroupOperator, FilterNode, FilterTypeValue, ItemField, ManagedFilterRule,
        PatternTemplate,
    },
};
use std::{rc::Rc, str::FromStr};
use web_sys::InputEvent;
use yew::{prelude::*, TargetCast};

fn selection_first(selection: &DropDownSelection) -> Option<&str> {
    match selection {
        DropDownSelection::Empty => None,
        DropDownSelection::Single(value) => Some(value.as_str()),
        DropDownSelection::Multi(values) => values.first().map(String::as_str),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TargetFilterBuilderResult {
    pub filter: ConfigTargetFilterDto,
    pub rules: Vec<ManagedFilterRule>,
}

#[derive(Properties, Clone, PartialEq)]
pub struct TargetFilterBuilderProps {
    pub filter: ConfigTargetFilterDto,
    #[prop_or_default]
    pub templates: Option<Vec<PatternTemplate>>,
    #[prop_or_default]
    pub rules: Option<Vec<ManagedFilterRule>>,
    pub on_result: Callback<Option<TargetFilterBuilderResult>>,
    #[prop_or_default]
    pub readonly: bool,
}

#[component]
pub fn TargetFilterBuilder(props: &TargetFilterBuilderProps) -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>();
    let initial_templates = props.templates.clone().unwrap_or_else(|| {
        config_ctx
            .as_ref()
            .and_then(|config| config.config.as_ref())
            .and_then(|config| config.templates.as_ref())
            .map_or_else(Vec::new, |definition| definition.templates.clone())
    });
    let initial_rules =
        props.rules.clone().unwrap_or_else(|| shared::foundation::managed_filter_rules(Some(&initial_templates)));
    let state = use_state(|| TargetFilterBuilderState::from_filter(&props.filter, initial_rules, initial_templates));
    let error = use_state(|| None::<String>);
    let dragging_rule = use_state(|| None::<usize>);
    let drag_over_rule = use_state(|| None::<usize>);
    let advanced_open = use_state(|| false);

    let dispatch = {
        let state = state.clone();
        let error = error.clone();
        Callback::from(move |action: FilterBuilderAction| match reduce_filter_builder(&state, action) {
            Ok(next) => {
                state.set(next);
                error.set(None);
            }
            Err(message) => error.set(Some(message)),
        })
    };

    let exclusions = state.exclusion_rules();
    let keep = state.exclusion_base();
    let processing = render_stage(
        FilterStage::Processing,
        state.processing.as_ref(),
        &state.rules,
        &state.templates,
        props.readonly,
        &dispatch,
        &translate,
    );
    let persist = render_stage(
        FilterStage::Persist,
        state.persist.as_ref(),
        &state.rules,
        &state.templates,
        props.readonly,
        &dispatch,
        &translate,
    );
    let library = render_rule_library(&state, props.readonly, &dispatch, &translate);
    let tabs = Rc::new(vec![
        TabItem {
            id: FilterStage::Persist.as_str().to_string(),
            title: translate.t("LABEL.PERSIST_FILTER"),
            icon: "Save".to_string(),
            children: persist,
            active_class: None,
            inactive_class: None,
        },
        TabItem {
            id: FilterStage::Processing.as_str().to_string(),
            title: translate.t("FILTER_BUILDER.PROCESSING_OPTIONAL"),
            icon: "Playlist".to_string(),
            children: processing,
            active_class: None,
            inactive_class: None,
        },
        TabItem {
            id: "library".to_string(),
            title: translate.t("FILTER_BUILDER.REUSABLE_OPTIONAL"),
            icon: "Nodes".to_string(),
            children: library,
            active_class: None,
            inactive_class: None,
        },
    ]);
    let compile_result = state.compile();
    let incomplete_rule = exclusions.iter().any(exclusion_is_incomplete);
    let valid = compile_result.is_ok() && !incomplete_rule;
    let actions = DialogActions {
        left: Some(vec![DialogAction::new(
            "cancel",
            "LABEL.CANCEL",
            DialogResult::Cancel,
            Some("Cancel".to_string()),
            Some("secondary".to_string()),
        )]),
        right: vec![DialogAction::new(
            "apply",
            "FILTER_BUILDER.APPLY_REMOVALS",
            DialogResult::Ok,
            Some("Accept".to_string()),
            Some("primary".to_string()),
        )
        .with_disabled(props.readonly || !valid)],
    };
    let on_confirm = {
        let state = state.clone();
        let on_result = props.on_result.clone();
        Callback::from(move |result: DialogResult| {
            if result == DialogResult::Ok {
                if state.exclusion_rules().iter().any(exclusion_is_incomplete) {
                    return;
                }
                match state.compile() {
                    Ok(filter) => on_result.emit(Some(TargetFilterBuilderResult { filter, rules: state.rules.clone() })),
                    Err(_) => on_result.emit(None),
                }
            } else {
                on_result.emit(None);
            }
        })
    };
    let add_removal = {
        let dispatch = dispatch.clone();
        Callback::from(move |_| dispatch.emit(FilterBuilderAction::AddExclusion(default_condition())))
    };
    let toggle_advanced = {
        let advanced_open = advanced_open.clone();
        Callback::from(move |_| advanced_open.set(!*advanced_open))
    };
    html! {
        <ContentDialog
            class="tp__filter-builder-dialog"
            content={html! {
                <section class="tp__filter-builder" aria-label={translate.t("FILTER_BUILDER.REMOVE_TITLE")}>
                    <div class="tp__filter-builder__workspace">
                        <div class="tp__filter-builder__hero">
                            <header class="tp__filter-builder__header">
                                <span class="tp__filter-builder__eyebrow">{ translate.t("FILTER_BUILDER.FINAL_OUTPUT") }</span>
                                <h2>{ translate.t("FILTER_BUILDER.REMOVE_TITLE") }</h2>
                                <p class="tp__filter-builder__intro">{ translate.t("FILTER_BUILDER.REMOVE_DESCRIPTION") }</p>
                            </header>
                            <div class="tp__filter-builder__stats">
                                <div class="tp__filter-builder__stat">
                                    <strong>{ exclusions.len() }</strong>
                                    <span>{ translate.t("FILTER_BUILDER.REMOVAL_COUNT").replace("{count}", &exclusions.len().to_string()) }</span>
                                </div>
                            </div>
                        </div>
                        if let Some(message) = error.as_ref().or_else(|| compile_result.as_ref().err()) {
                            <div class="tp__filter-builder__error" role="alert">{ message.clone() }</div>
                        } else if incomplete_rule {
                            <div class="tp__filter-builder__error" role="alert">{ translate.t("FILTER_BUILDER.RULE_VALUE") }</div>
                        }
                        <section class="tp__filter-builder__section" aria-label={translate.t("FILTER_BUILDER.REMOVE_WHEN")}>
                            <div class="tp__filter-builder__section-heading">
                                <div>
                                    <h3 class="tp__filter-builder__section-title">{ translate.t("FILTER_BUILDER.REMOVE_WHEN") }</h3>
                                    <p class="tp__filter-builder__section-description">{ translate.t("FILTER_BUILDER.MATCHING_ITEMS_REMOVED") }</p>
                                </div>
                                <span class="tp__filter-builder__badge">{ translate.t("FILTER_BUILDER.FINAL_OUTPUT") }</span>
                            </div>
                            if let Some(keep) = keep.as_ref() {
                                <div class="tp__filter-builder__summary" role="status">
                                    <div class="tp__filter-builder__summary-stage">
                                        <strong>{ translate.t("FILTER_BUILDER.LEGACY_KEEP") }</strong>
                                        <span>{ translate.t("FILTER_BUILDER.LEGACY_KEEP_HINT") }</span>
                                    </div>
                                    <code>{ compile_filter_draft_with_templates(Some(keep), &state.rules, Some(&state.templates)).ok().flatten().unwrap_or_default() }</code>
                                </div>
                            }
                            if exclusions.is_empty() {
                                <div class="tp__filter-builder__empty-state">
                                    <AppIcon name="Playlist" width="28px" height="28px" />
                                    <strong>{ translate.t("FILTER_BUILDER.NO_REMOVALS") }</strong>
                                    <span>{ translate.t("FILTER_BUILDER.NO_REMOVALS_DETAIL") }</span>
                                </div>
                            } else {
                                <div class="tp__filter-builder__rule-list">
                                    { for exclusions.iter().enumerate().map(|(index, rule)| render_exclusion_rule(index, exclusions.len(), rule, props.readonly, &dispatch, &dragging_rule, &drag_over_rule, &state.rules, &state.templates, &translate)) }
                                </div>
                            }
                            if !props.readonly {
                                <button type="button" class="tp__filter-builder__add-rule" onclick={add_removal}>
                                    <AppIcon name="Add" />{ translate.t("FILTER_BUILDER.ADD_REMOVAL") }
                                </button>
                            }
                        </section>
                        <section class="tp__filter-builder__secondary">
                            <button type="button" class="tp__filter-builder__advanced-toggle" aria-expanded={(*advanced_open).to_string()} onclick={toggle_advanced}>
                                <AppIcon name="Options" />{ translate.t("FILTER_BUILDER.OPTIONAL_ADVANCED") }
                            </button>
                            if *advanced_open {
                                <div class="tp__filter-builder__advanced-content">
                                    <p>{ translate.t("FILTER_BUILDER.ADVANCED_HINT") }</p>
                                    <TabSet tabs={tabs} class="tp__filter-builder__tabs" />
                                </div>
                            }
                        </section>
                    </div>
                </section>
            }}
            actions={actions}
            close_on_backdrop_click={false}
            on_confirm={on_confirm}
        />
    }
}

fn exclusion_is_incomplete(node: &FilterNode) -> bool {
    match node {
        FilterNode::String { value, .. } => value.trim().is_empty(),
        FilterNode::Regex { pattern, .. } => pattern.trim().is_empty(),
        FilterNode::Group { children, .. } => children.is_empty() || children.iter().any(exclusion_is_incomplete),
        FilterNode::Not { node } => exclusion_is_incomplete(node),
        FilterNode::Raw { expression } => expression.trim().is_empty(),
        FilterNode::Set { values, .. } => values.is_empty(),
        _ => false,
    }
}

fn render_exclusion_rule(
    index: usize,
    total: usize,
    rule: &FilterNode,
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    dragging: &UseStateHandle<Option<usize>>,
    drag_over: &UseStateHandle<Option<usize>>,
    rules: &[ManagedFilterRule],
    templates: &[PatternTemplate],
    translate: &crate::i18n::YewI18n,
) -> Html {
    let path = vec![index];
    let on_drag_start = {
        let dragging = dragging.clone();
        Callback::from(move |event: DragEvent| {
            if let Some(transfer) = event.data_transfer() {
                let _ = transfer.set_data("text/plain", &format!("target-filter-rule:{index}"));
            }
            dragging.set(Some(index));
        })
    };
    let on_drag_over = {
        let drag_over = drag_over.clone();
        Callback::from(move |event: DragEvent| {
            event.prevent_default();
            drag_over.set(Some(index));
        })
    };
    let on_drop = {
        let dragging = dragging.clone();
        let drag_over = drag_over.clone();
        let dispatch = dispatch.clone();
        Callback::from(move |event: DragEvent| {
            event.prevent_default();
            if let Some(from) = *dragging {
                if event.data_transfer().and_then(|transfer| transfer.get_data("text/plain").ok())
                    == Some(format!("target-filter-rule:{from}")) && from != index {
                    dispatch.emit(FilterBuilderAction::MoveExclusion(vec![from], vec![index]));
                }
            }
            dragging.set(None);
            drag_over.set(None);
        })
    };
    let on_drag_end = {
        let dragging = dragging.clone();
        let drag_over = drag_over.clone();
        Callback::from(move |_: DragEvent| {
            dragging.set(None);
            drag_over.set(None);
        })
    };
    let action = |icon: &'static str, label: String, action: FilterBuilderAction, disabled: bool| {
        let dispatch = dispatch.clone();
        html! {
            <button type="button" class="tp__filter-builder__icon-action" title={label.clone()} aria-label={label}
                disabled={disabled} onclick={Callback::from(move |_| dispatch.emit(action.clone()))}>
                <AppIcon name={icon} />
            </button>
        }
    };
    html! {
        <div class={classes!("tp__filter-builder__rule-card", (**dragging == Some(index)).then_some("dragging"), (**drag_over == Some(index)).then_some("drag-over"))}
            ondragover={on_drag_over} ondrop={on_drop}>
            if !readonly {
                <span class="tp__filter-builder__rule-handle" draggable="true" ondragstart={on_drag_start} ondragend={on_drag_end}
                    title={translate.t("FILTER_BUILDER.DRAG_RULE")} aria-label={translate.t("FILTER_BUILDER.DRAG_RULE")}>
                    <AppIcon name="Handle" />
                </span>
            }
            <span class="tp__filter-builder__rule-number">{ index + 1 }</span>
            <div class="tp__filter-builder__rule-content">
                <div class="tp__filter-builder__rule-heading">
                    <strong>{ translate.t("FILTER_BUILDER.REMOVE_WHEN") }</strong>
                    if !readonly {
                        <div class="tp__filter-builder__rule-actions">
                            { action("ArrowUp", translate.t("FILTER_BUILDER.MOVE_RULE_UP"), FilterBuilderAction::MoveExclusion(path.clone(), vec![index.saturating_sub(1)]), index == 0) }
                            { action("ArrowDown", translate.t("FILTER_BUILDER.MOVE_RULE_DOWN"), FilterBuilderAction::MoveExclusion(path.clone(), vec![(index + 1).min(total - 1)]), index + 1 == total) }
                            { action("Copy", translate.t("FILTER_BUILDER.DUPLICATE_RULE"), FilterBuilderAction::DuplicateExclusion(path.clone()), false) }
                            { action("Delete", translate.t("FILTER_BUILDER.REMOVE_RULE"), FilterBuilderAction::RemoveExclusion(path), false) }
                        </div>
                    }
                </div>
                { render_exclusion_fields(index, rule, readonly, dispatch, rules, templates, translate) }
            </div>
        </div>
    }
}

fn render_exclusion_fields(
    index: usize,
    rule: &FilterNode,
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    rules: &[ManagedFilterRule],
    templates: &[PatternTemplate],
    translate: &crate::i18n::YewI18n,
) -> Html {
    let (field, operator, value) = match rule {
        FilterNode::String { field, operator, value } => (*field, string_operator_id(*operator), value.clone()),
        FilterNode::Regex { field, pattern } => (*field, "regex", pattern.clone()),
        FilterNode::Type { value } => (ItemField::Type, "eq", match value {
            FilterTypeValue::Live => "live", FilterTypeValue::Movie => "movie", FilterTypeValue::Series => "series",
        }.to_string()),
        FilterNode::Number { field, operator, value } => (*field, match operator {
            NumericOperator::Eq => "eq", NumericOperator::NotEq => "not_eq", NumericOperator::Greater => "gt",
            NumericOperator::GreaterOrEqual => "gte", NumericOperator::Less => "lt",
            NumericOperator::LessOrEqual => "lte",
        }, value.to_string()),
        _ => return html! {
            <div class="tp__filter-builder__secondary">
                <span>{ translate.t("FILTER_BUILDER.OPTIONAL_ADVANCED") }</span>
                <code>{ compile_filter_draft_with_templates(Some(rule), rules, Some(templates)).ok().flatten().unwrap_or_default() }</code>
            </div>
        },
    };
    let fields = [ItemField::Group, ItemField::Name, ItemField::Title, ItemField::Caption,
        ItemField::Genre, ItemField::Input, ItemField::EpgId, ItemField::Url, ItemField::Type,
        ItemField::Chno, ItemField::Quality];
    let on_field = {
        let dispatch = dispatch.clone();
        Callback::from(move |event: Event| {
            if let Some(select) = event.target_dyn_into::<web_sys::HtmlSelectElement>() {
                if let Ok(field) = ItemField::from_str(&select.value()) {
                    dispatch.emit(FilterBuilderAction::ReplaceExclusion(vec![index], default_node_for_field(field)));
                }
            }
        })
    };
    let on_operator = {
        let dispatch = dispatch.clone();
        let value = value.clone();
        Callback::from(move |event: Event| {
            if let Some(select) = event.target_dyn_into::<web_sys::HtmlSelectElement>() {
                let selected = select.value();
                let next = if field == ItemField::Type {
                    return;
                } else if matches!(field, ItemField::Chno | ItemField::Quality) {
                    let op = match selected.as_str() {
                        "not_eq" => NumericOperator::NotEq, "gt" => NumericOperator::Greater,
                        "gte" => NumericOperator::GreaterOrEqual, "lt" => NumericOperator::Less,
                        "lte" => NumericOperator::LessOrEqual, _ => NumericOperator::Eq,
                    };
                    FilterNode::Number { field, operator: op, value: value.parse().unwrap_or_default() }
                } else if selected == "regex" {
                    FilterNode::Regex { field, pattern: value.clone() }
                } else {
                    FilterNode::String { field, operator: parse_string_operator(&selected).unwrap_or(StringOperator::Eq), value: value.clone() }
                };
                dispatch.emit(FilterBuilderAction::ReplaceExclusion(vec![index], next));
            }
        })
    };
    let on_value = {
        let dispatch = dispatch.clone();
        let operator = operator.to_string();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event.target_dyn_into::<web_sys::HtmlInputElement>() {
                let value = input.value();
                let next = if field == ItemField::Type {
                    FilterNode::Type { value: match value.as_str() {
                        "movie" => FilterTypeValue::Movie, "series" => FilterTypeValue::Series,
                        _ => FilterTypeValue::Live,
                    }}
                } else if matches!(field, ItemField::Chno | ItemField::Quality) {
                    let Ok(number) = value.parse::<u32>() else { return; };
                    let op = match operator.as_str() {
                        "not_eq" => NumericOperator::NotEq, "gt" => NumericOperator::Greater,
                        "gte" => NumericOperator::GreaterOrEqual, "lt" => NumericOperator::Less,
                        "lte" => NumericOperator::LessOrEqual, _ => NumericOperator::Eq,
                    };
                    FilterNode::Number { field, operator: op, value: number }
                } else if operator == "regex" {
                    FilterNode::Regex { field, pattern: value }
                } else {
                    FilterNode::String { field, operator: parse_string_operator(&operator).unwrap_or(StringOperator::Eq), value }
                };
                dispatch.emit(FilterBuilderAction::ReplaceExclusion(vec![index], next));
            }
        })
    };
    let numeric = matches!(field, ItemField::Chno | ItemField::Quality);
    let typed = field == ItemField::Type;
    let on_type = {
        let dispatch = dispatch.clone();
        Callback::from(move |event: Event| {
            if let Some(select) = event.target_dyn_into::<web_sys::HtmlSelectElement>() {
                let value = match select.value().as_str() {
                    "movie" => FilterTypeValue::Movie, "series" => FilterTypeValue::Series,
                    _ => FilterTypeValue::Live,
                };
                dispatch.emit(FilterBuilderAction::ReplaceExclusion(vec![index], FilterNode::Type { value }));
            }
        })
    };
    html! {
        <div class="tp__filter-builder__rule-fields">
            <label>{ translate.t("FILTER_BUILDER.RULE_FIELD") }
                <select class="tp__input" aria-label={translate.t("FILTER_BUILDER.RULE_FIELD")}
                    value={field.to_string()} onchange={on_field} disabled={readonly}>
                    { for fields.into_iter().map(|candidate| html! {
                        <option value={candidate.to_string()} selected={candidate == field}>{ candidate.to_string() }</option>
                    }) }
                </select>
            </label>
            if !typed {
                <label>{ translate.t("FILTER_BUILDER.RULE_OPERATOR") }
                    <select class="tp__input" aria-label={translate.t("FILTER_BUILDER.RULE_OPERATOR")}
                        value={operator} onchange={on_operator} disabled={readonly}>
                        if numeric {
                            { for [("eq", "="), ("not_eq", "≠"), ("gt", ">"), ("gte", "≥"), ("lt", "<"), ("lte", "≤")].into_iter().map(|(id, name)| html! {
                                <option value={id} selected={id == operator}>{ name }</option>
                            }) }
                        } else {
                            { for [("eq", "="), ("not_eq", "≠"), ("contains", "CONTAINS"), ("starts_with", "STARTSWITH"), ("regex", "~")].into_iter().map(|(id, name)| html! {
                                <option value={id} selected={id == operator}>{ name }</option>
                            }) }
                        }
                    </select>
                </label>
            }
            <label>{ translate.t("FILTER_BUILDER.RULE_VALUE") }
                if typed {
                    <select class="tp__input" aria-label={translate.t("FILTER_BUILDER.RULE_VALUE")}
                        onchange={on_type} disabled={readonly}>
                        { for [("live", "Live"), ("movie", "Movie"), ("series", "Series")].into_iter().map(|(id, label)| html! {
                            <option value={id} selected={id == value}>{ label }</option>
                        }) }
                    </select>
                } else {
                    <input class="tp__input" type={if numeric { "number" } else { "text" }}
                        min={if numeric { Some("0") } else { None }} value={value}
                        aria-label={translate.t("FILTER_BUILDER.RULE_VALUE")}
                        placeholder={if field == ItemField::Group { translate.t("FILTER_BUILDER.CATEGORY_NAME") } else { translate.t("FILTER_BUILDER.RULE_VALUE") }}
                        oninput={on_value} disabled={readonly} />
                }
            </label>
        </div>
    }
}

fn render_stage(
    stage: FilterStage,
    root: Option<&FilterNode>,
    rules: &[ManagedFilterRule],
    templates: &[PatternTemplate],
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let hint = match stage {
        FilterStage::Processing => "FILTER_BUILDER.PROCESSING_HINT",
        FilterStage::Persist => "FILTER_BUILDER.PERSIST_HINT",
    };
    html! {
        <div class="tp__filter-builder__stage">
            <p class="tp__filter-builder__hint">{ translate.t(hint) }</p>
            {
                root.map_or_else(
                    || {
                        let dispatch = dispatch.clone();
                        html! {
                            <div class="tp__filter-builder__empty">
                                <span>{ translate.t("FILTER_BUILDER.EMPTY") }</span>
                                if !readonly {
                                    <TextButton
                                        name="create-filter"
                                        icon="Add"
                                        title={translate.t("FILTER_BUILDER.CREATE_FILTER")}
                                        onclick={Callback::from(move |_| dispatch.emit(FilterBuilderAction::ReplaceRoot(stage, Some(default_group()))))}
                                    />
                                }
                            </div>
                        }
                    },
                    |root| render_node(stage, root, &Vec::new(), rules, templates, readonly, dispatch, translate),
                )
            }
            if !readonly && root.is_some() {
                <TextButton
                    name="clear-filter"
                    icon="Delete"
                    title={translate.t("FILTER_BUILDER.CLEAR_STAGE")}
                    class="danger"
                    onclick={{
                        let dispatch = dispatch.clone();
                        Callback::from(move |_| dispatch.emit(FilterBuilderAction::ReplaceRoot(stage, None)))
                    }}
                />
            }
            { render_preview(root, rules, templates, translate) }
        </div>
    }
}

fn render_preview(
    root: Option<&FilterNode>,
    rules: &[ManagedFilterRule],
    templates: &[PatternTemplate],
    translate: &crate::i18n::YewI18n,
) -> Html {
    let Ok(Some(expression)) = compile_filter_draft_with_templates(root, rules, Some(templates)) else {
        return Html::default();
    };
    let prepared_templates = shared::foundation::prepare_filter_builder_templates(rules, Some(templates)).ok();
    let parsed = shared::foundation::get_filter(&expression, prepared_templates.as_deref()).ok();
    html! {
        <details class="tp__filter-builder__preview">
            <summary>{ translate.t("FILTER_BUILDER.PREVIEW") }</summary>
            <FilterView pretty={true} filter={parsed} />
            <code>{ expression }</code>
        </details>
    }
}

fn render_node(
    stage: FilterStage,
    node: &FilterNode,
    path: &NodePath,
    rules: &[ManagedFilterRule],
    templates: &[PatternTemplate],
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let body = match node {
        FilterNode::Group { operator, children } => {
            let group_options = Rc::new(vec![
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
            let on_group_select = {
                let dispatch = dispatch.clone();
                let path = path.clone();
                let children = children.clone();
                Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    let operator = if selection_first(&selection) == Some("any") {
                        FilterGroupOperator::Any
                    } else {
                        FilterGroupOperator::All
                    };
                    dispatch.emit(FilterBuilderAction::ReplaceNode(
                        stage,
                        path.clone(),
                        FilterNode::Group { operator, children: children.clone() },
                    ));
                })
            };
            html! {
                <div class="tp__filter-builder__group">
                    <div class="tp__filter-builder__group-heading">
                        <strong>{ translate.t("FILTER_BUILDER.MATCH") }</strong>
                        <Select name={format!("group-{}", path_key(path))} options={group_options} multi_select={false} on_select={on_group_select} />
                    </div>
                    <div class="tp__filter-builder__children">
                        { for children.iter().enumerate().map(|(index, child)| {
                            let mut child_path = path.clone();
                            child_path.push(index);
                            render_node(stage, child, &child_path, rules, templates, readonly, dispatch, translate)
                        }) }
                    </div>
                    if !readonly {
                        <div class="tp__filter-builder__add-row">
                            { add_child_buttons(stage, path, rules, templates, dispatch, translate) }
                        </div>
                    }
                </div>
            }
        }
        FilterNode::Not { node } => {
            let mut child_path = path.clone();
            child_path.push(0);
            html! {
                <div class="tp__filter-builder__not">
                    <strong>{ translate.t("FILTER_BUILDER.NOT") }</strong>
                    { render_node(stage, node, &child_path, rules, templates, readonly, dispatch, translate) }
                </div>
            }
        }
        FilterNode::LegacyReference { template } => {
            render_legacy_reference(stage, path, template, templates, dispatch, translate)
        }
        FilterNode::Reference { rule_id } => {
            render_reference(stage, path, rule_id, rules, readonly, dispatch, translate)
        }
        FilterNode::Raw { expression } => render_raw(stage, path, expression, readonly, dispatch, translate),
        _ => render_condition(stage, path, node, readonly, dispatch, translate),
    };

    html! {
        <Card class="tp__filter-builder__node">
            { body }
            if !readonly {
                { node_actions(stage, path, dispatch, translate) }
            }
        </Card>
    }
}

fn add_child_buttons(
    stage: FilterStage,
    path: &NodePath,
    rules: &[ManagedFilterRule],
    templates: &[PatternTemplate],
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let condition = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |_| dispatch.emit(FilterBuilderAction::AddChild(stage, path.clone(), default_condition())))
    };
    let group = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |_| dispatch.emit(FilterBuilderAction::AddChild(stage, path.clone(), default_group())))
    };
    let raw = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |_| {
            dispatch.emit(FilterBuilderAction::AddChild(
                stage,
                path.clone(),
                FilterNode::Raw { expression: String::new() },
            ));
        })
    };
    let rule_options =
        Rc::new(rules.iter().map(|rule| DropDownOption::new(&rule.id, html! { rule.name.clone() }, false)).collect());
    let add_rule = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(rule_id) = selection_first(&selection) {
                dispatch.emit(FilterBuilderAction::AddChild(
                    stage,
                    path.clone(),
                    FilterNode::Reference { rule_id: rule_id.to_string() },
                ));
            }
        })
    };
    let legacy_templates = templates
        .iter()
        .filter(|template| !template.is_managed_filter_rule())
        .map(|template| template.name.clone())
        .collect::<Vec<_>>();
    let template_options = Rc::new(
        legacy_templates
            .iter()
            .map(|name| DropDownOption::new(name, html! { name.clone() }, false))
            .collect::<Vec<_>>(),
    );
    let add_template = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(template) = selection_first(&selection) {
                dispatch.emit(FilterBuilderAction::AddChild(
                    stage,
                    path.clone(),
                    FilterNode::LegacyReference { template: template.to_string() },
                ));
            }
        })
    };
    html! {
        <>
            <TextButton name="add-condition" icon="Add" title={translate.t("FILTER_BUILDER.ADD_CONDITION")} onclick={condition} />
            <TextButton name="add-group" icon="Nodes" title={translate.t("FILTER_BUILDER.ADD_GROUP")} onclick={group} />
            <TextButton name="add-advanced" icon="Edit" title={translate.t("FILTER_BUILDER.ADD_ADVANCED")} onclick={raw} />
            if !rules.is_empty() {
                <Select name={format!("add-rule-{}", path_key(path))} options={rule_options} multi_select={false} on_select={add_rule} />
            }
            if !template_options.is_empty() {
                <Select name={format!("add-template-{}", path_key(path))} options={template_options} multi_select={false} on_select={add_template} />
            }
        </>
    }
}

fn node_actions(
    stage: FilterStage,
    path: &NodePath,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let action = |name: &'static str, icon: &'static str, title: String, action: FilterBuilderAction| {
        let dispatch = dispatch.clone();
        html! { <TextButton name={name} icon={icon} title={title} onclick={Callback::from(move |_| dispatch.emit(action.clone()))} /> }
    };
    html! {
        <div class="tp__filter-builder__node-actions">
            { action("move-up", "ArrowUp", translate.t("LABEL.MOVE_UP"), FilterBuilderAction::MoveNode(stage, path.clone(), -1)) }
            { action("move-down", "ArrowDown", translate.t("LABEL.MOVE_DOWN"), FilterBuilderAction::MoveNode(stage, path.clone(), 1)) }
            { action("duplicate", "Copy", translate.t("FILTER_BUILDER.DUPLICATE"), FilterBuilderAction::DuplicateNode(stage, path.clone())) }
            { action("toggle-not", "Clear", translate.t("FILTER_BUILDER.TOGGLE_NOT"), FilterBuilderAction::ToggleNot(stage, path.clone())) }
            { action("wrap-all", "Nodes", translate.t("FILTER_BUILDER.WRAP_GROUP"), FilterBuilderAction::WrapNode(stage, path.clone(), FilterGroupOperator::All)) }
            { action("remove", "Delete", translate.t("LABEL.DELETE"), FilterBuilderAction::RemoveNode(stage, path.clone())) }
        </div>
    }
}

fn render_condition(
    stage: FilterStage,
    path: &NodePath,
    node: &FilterNode,
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let field = condition_field(node);
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
    let field_options = Rc::new(build_options(fields, &field, |value| html! { value.to_string() }));
    let on_field = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(value) = selection_first(&selection).and_then(|value| ItemField::from_str(value).ok()) {
                dispatch.emit(FilterBuilderAction::ReplaceNode(stage, path.clone(), default_node_for_field(value)));
            }
        })
    };
    html! {
        <div class="tp__filter-builder__condition">
            <Select name={format!("field-{}", path_key(path))} options={field_options} multi_select={false} on_select={on_field} />
            { render_condition_value(stage, path, node, readonly, dispatch, translate) }
        </div>
    }
}

fn condition_field(node: &FilterNode) -> ItemField {
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

fn render_condition_value(
    stage: FilterStage,
    path: &NodePath,
    node: &FilterNode,
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    match node {
        FilterNode::String { field, operator, value } => {
            let operators =
                [StringOperator::Eq, StringOperator::NotEq, StringOperator::Contains, StringOperator::StartsWith];
            let options = Rc::new(
                operators
                    .into_iter()
                    .map(|candidate| {
                        DropDownOption::new(
                            string_operator_id(candidate),
                            html! { candidate.to_string() },
                            candidate == *operator,
                        )
                    })
                    .collect(),
            );
            let on_operator = {
                let dispatch = dispatch.clone();
                let path = path.clone();
                let field = *field;
                let value = value.clone();
                Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    if let Some(operator) = selection_first(&selection).and_then(parse_string_operator) {
                        dispatch.emit(FilterBuilderAction::ReplaceNode(
                            stage,
                            path.clone(),
                            FilterNode::String { field, operator, value: value.clone() },
                        ));
                    }
                })
            };
            let on_value = replace_string_value(stage, path, *field, *operator, dispatch);
            html! {
                <>
                    <Select name={format!("operator-{}", path_key(path))} options={options} multi_select={false} on_select={on_operator} />
                    <Input name={format!("value-{}", path_key(path))} value={value.clone()} on_change={Some(on_value)} aria_label={Some(translate.t("FILTER_BUILDER.VALUE"))} />
                </>
            }
        }
        FilterNode::Regex { field, pattern } => {
            let field = *field;
            let path = path.clone();
            let dispatch = dispatch.clone();
            let pattern_value = pattern.clone();
            html! {
                <>
                    <span class="tp__filter-builder__operator">{"~"}</span>
                    <Input name={format!("regex-{}", path_key(&path))} value={pattern_value} on_change={Some(Callback::from(move |value| {
                        dispatch.emit(FilterBuilderAction::ReplaceNode(stage, path.clone(), FilterNode::Regex { field, pattern: value }));
                    }))} aria_label={Some(translate.t("FILTER_BUILDER.REGEX"))} />
                </>
            }
        }
        FilterNode::Presence { field, operator } => {
            let options = Rc::new(vec![
                DropDownOption::new(
                    "is_empty",
                    html! { translate.t("FILTER_BUILDER.IS_EMPTY") },
                    *operator == PresenceOperator::IsEmpty,
                ),
                DropDownOption::new(
                    "is_not_empty",
                    html! { translate.t("FILTER_BUILDER.IS_NOT_EMPTY") },
                    *operator == PresenceOperator::IsNotEmpty,
                ),
            ]);
            let dispatch = dispatch.clone();
            let path = path.clone();
            let field = *field;
            html! {
                <Select name={format!("presence-{}", path_key(&path))} options={options} multi_select={false} on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    let operator = if selection_first(&selection) == Some("is_not_empty") { PresenceOperator::IsNotEmpty } else { PresenceOperator::IsEmpty };
                    dispatch.emit(FilterBuilderAction::ReplaceNode(stage, path.clone(), FilterNode::Presence { field, operator }));
                })} />
            }
        }
        FilterNode::Number { field, operator, value } => {
            render_number(stage, path, *field, *operator, *value, dispatch, translate)
        }
        FilterNode::Set { field, values } => {
            let dispatch = dispatch.clone();
            let path = path.clone();
            let field = *field;
            html! {
                <>
                    <span class="tp__filter-builder__operator">{"IN"}</span>
                    <Input name={format!("set-{}", path_key(&path))} value={values.join(", ")} on_change={Some(Callback::from(move |value: String| {
                        let values = value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_string).collect();
                        dispatch.emit(FilterBuilderAction::ReplaceNode(stage, path.clone(), FilterNode::Set { field, values }));
                    }))} aria_label={Some(translate.t("FILTER_BUILDER.VALUES"))} />
                </>
            }
        }
        FilterNode::Type { value } => {
            let options = Rc::new(vec![
                DropDownOption::new(
                    "live",
                    html! { translate.t("FILTER_BUILDER.TYPE_LIVE") },
                    *value == FilterTypeValue::Live,
                ),
                DropDownOption::new(
                    "movie",
                    html! { translate.t("FILTER_BUILDER.TYPE_MOVIE") },
                    *value == FilterTypeValue::Movie,
                ),
                DropDownOption::new(
                    "series",
                    html! { translate.t("FILTER_BUILDER.TYPE_SERIES") },
                    *value == FilterTypeValue::Series,
                ),
            ]);
            let dispatch = dispatch.clone();
            let path = path.clone();
            html! {
                <Select name={format!("type-{}", path_key(&path))} options={options} multi_select={false} on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    let value = match selection_first(&selection) {
                        Some("movie") => FilterTypeValue::Movie,
                        Some("series") => FilterTypeValue::Series,
                        _ => FilterTypeValue::Live,
                    };
                    dispatch.emit(FilterBuilderAction::ReplaceNode(stage, path.clone(), FilterNode::Type { value }));
                })} />
            }
        }
        _ => {
            html! { <span>{ if readonly { String::new() } else { translate.t("FILTER_BUILDER.INVALID_CONDITION") } }</span> }
        }
    }
}

fn replace_string_value(
    stage: FilterStage,
    path: &NodePath,
    field: ItemField,
    operator: StringOperator,
    dispatch: &Callback<FilterBuilderAction>,
) -> Callback<String> {
    let path = path.clone();
    let dispatch = dispatch.clone();
    Callback::from(move |value| {
        dispatch.emit(FilterBuilderAction::ReplaceNode(
            stage,
            path.clone(),
            FilterNode::String { field, operator, value },
        ));
    })
}

fn render_number(
    stage: FilterStage,
    path: &NodePath,
    field: ItemField,
    operator: NumericOperator,
    value: u32,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
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
            .map(|(id, candidate)| DropDownOption::new(id, html! { candidate.to_string() }, *candidate == operator))
            .collect(),
    );
    let on_operator = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some((_, operator)) = operators.iter().find(|(id, _)| Some(*id) == selection_first(&selection)) {
                dispatch.emit(FilterBuilderAction::ReplaceNode(
                    stage,
                    path.clone(),
                    FilterNode::Number { field, operator: *operator, value },
                ));
            }
        })
    };
    let on_value = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event.target_dyn_into::<web_sys::HtmlInputElement>() {
                if let Ok(value) = input.value().parse::<u32>() {
                    dispatch.emit(FilterBuilderAction::ReplaceNode(
                        stage,
                        path.clone(),
                        FilterNode::Number { field, operator, value },
                    ));
                }
            }
        })
    };
    html! {
        <>
            <Select name={format!("number-op-{}", path_key(path))} options={options} multi_select={false} on_select={on_operator} />
            <input type="number" min="0" value={value.to_string()} oninput={on_value} aria-label={translate.t("FILTER_BUILDER.VALUE")} />
        </>
    }
}

fn render_legacy_reference(
    stage: FilterStage,
    path: &NodePath,
    selected: &str,
    templates: &[PatternTemplate],
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let options = Rc::new(
        templates
            .iter()
            .filter(|template| !template.is_managed_filter_rule())
            .map(|template| {
                DropDownOption::new(&template.name, html! { template.name.clone() }, template.name == selected)
            })
            .collect::<Vec<_>>(),
    );
    let on_select = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(template) = selection_first(&selection) {
                dispatch.emit(FilterBuilderAction::ReplaceNode(
                    stage,
                    path.clone(),
                    FilterNode::LegacyReference { template: template.to_string() },
                ));
            }
        })
    };
    let value = templates
        .iter()
        .find(|template| template.name == selected)
        .map(|template| template.value.to_string())
        .unwrap_or_default();
    html! {
        <div class="tp__filter-builder__reference">
            <strong>{ translate.t("FILTER_BUILDER.TEMPLATE") }</strong>
            <Select name={format!("legacy-reference-{}", path_key(path))} options={options} multi_select={false} on_select={on_select} />
            <code class="tp__filter-builder__reference-value">{ value }</code>
        </div>
    }
}

fn render_reference(
    stage: FilterStage,
    path: &NodePath,
    rule_id: &str,
    rules: &[ManagedFilterRule],
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let options = Rc::new(
        rules
            .iter()
            .map(|rule| DropDownOption::new(&rule.id, html! { rule.name.clone() }, rule.id == rule_id))
            .collect(),
    );
    let on_select = {
        let dispatch = dispatch.clone();
        let path = path.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(rule_id) = selection_first(&selection) {
                dispatch.emit(FilterBuilderAction::ReplaceNode(
                    stage,
                    path.clone(),
                    FilterNode::Reference { rule_id: rule_id.to_string() },
                ));
            }
        })
    };
    html! {
        <div class="tp__filter-builder__reference">
            <strong>{ translate.t("FILTER_BUILDER.SHARED_RULE") }</strong>
            <Select name={format!("reference-{}", path_key(path))} options={options} multi_select={false} on_select={on_select} />
            if !readonly {
                <span>{ translate.t("FILTER_BUILDER.LINKED") }</span>
            }
        </div>
    }
}

fn render_raw(
    stage: FilterStage,
    path: &NodePath,
    expression: &str,
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let on_change = {
        let path = path.clone();
        let dispatch = dispatch.clone();
        Callback::from(move |expression| {
            dispatch.emit(FilterBuilderAction::ReplaceNode(stage, path.clone(), FilterNode::Raw { expression }));
        })
    };
    html! {
        <details class="tp__filter-builder__raw" open={true}>
            <summary>{ translate.t("FILTER_BUILDER.ADVANCED") }</summary>
            if readonly {
                <code>{ expression }</code>
            } else {
                <TextArea name={format!("raw-{}", path_key(path))} value={expression.to_string()} on_change={Some(on_change)} rows={Some(4)} />
            }
        </details>
    }
}

fn render_rule_library(
    state: &TargetFilterBuilderState,
    readonly: bool,
    dispatch: &Callback<FilterBuilderAction>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let new_rule_name = translate.t("FILTER_BUILDER.NEW_RULE_NAME");
    let add = {
        let dispatch = dispatch.clone();
        Callback::from(move |_| dispatch.emit(FilterBuilderAction::AddRule(new_rule_name.clone())))
    };
    let translate = translate.clone();
    html! {
        <div class="tp__filter-builder__library">
            <p>{ translate.t("FILTER_BUILDER.SHARED_RULES_HINT") }</p>
            { for state.rules.iter().map(|rule| {
                let id = rule.id.clone();
                let usage_count = state.rule_usage_count(&id);
                let rename = {
                    let dispatch = dispatch.clone();
                    let id = id.clone();
                    Callback::from(move |name| dispatch.emit(FilterBuilderAction::RenameRule(id.clone(), name)))
                };
                let remove = {
                    let dispatch = dispatch.clone();
                    let id = id.clone();
                    Callback::from(move |_| dispatch.emit(FilterBuilderAction::RemoveRule(id.clone())))
                };
                let replace_root = {
                    let dispatch = dispatch.clone();
                    let id = id.clone();
                    Callback::from(move |root| dispatch.emit(FilterBuilderAction::ReplaceRuleRoot(id.clone(), root)))
                };
                html! {
                    <Card class="tp__filter-builder__library-rule">
                        <div class="tp__filter-builder__library-rule-heading">
                            <Input name={format!("rule-name-{id}")} value={rule.name.clone()} on_change={Some(rename)} aria_label={Some(translate.t("FILTER_BUILDER.RULE_NAME"))} />
                            <span>{ format!("{} {}", usage_count, translate.t("FILTER_BUILDER.USES")) }</span>
                            if !readonly {
                                <TextButton name="remove-rule" icon="Delete" title={translate.t("LABEL.DELETE")} disabled={usage_count > 0} onclick={remove} />
                            }
                        </div>
                        <FilterRuleTreeEditor rule={rule.clone()} all_rules={state.rules.clone()} on_change={replace_root} readonly={readonly} />
                        <details>
                            <summary>{ translate.t("FILTER_BUILDER.COMPILED_EXPRESSION") }</summary>
                            <code>{ compile_filter_draft_with_templates(Some(&rule.root), &state.rules, Some(&state.templates)).ok().flatten().unwrap_or_default() }</code>
                        </details>
                    </Card>
                }
            }) }
            if !readonly {
                <TextButton name="add-rule" icon="Add" title={translate.t("FILTER_BUILDER.ADD_SHARED_RULE")} onclick={add} />
            }
        </div>
    }
}

fn path_key(path: &[usize]) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.iter().map(usize::to_string).collect::<Vec<_>>().join("-")
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct TargetFilterSummaryProps {
    pub filter: ConfigTargetFilterDto,
    #[prop_or_default]
    pub templates: Option<Vec<PatternTemplate>>,
    #[prop_or_default]
    pub rules: Option<Vec<ManagedFilterRule>>,
    #[prop_or_default]
    pub onclick: Option<Callback<MouseEvent>>,
    #[prop_or_default]
    pub readonly: bool,
}

#[component]
pub fn TargetFilterSummary(props: &TargetFilterSummaryProps) -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>();
    let templates = props.templates.clone().unwrap_or_else(|| {
        config_ctx
            .as_ref()
            .and_then(|config| config.config.as_ref())
            .and_then(|config| config.templates.as_ref())
            .map_or_else(Vec::new, |definition| definition.templates.clone())
    });
    let rules = props.rules.clone().unwrap_or_else(|| shared::foundation::managed_filter_rules(Some(&templates)));
    let processing = import_preview(props.filter.processing.as_deref(), &rules, &templates, &translate);
    let persist_root = shared::foundation::import_filter_draft_with_templates(props.filter.persist.as_deref(), &rules, Some(&templates));
    let exclusions = super::filter_builder_model::extract_persist_exclusions(persist_root.as_ref());
    let has_advanced = exclusions.keep.is_some();
    let count = exclusions.rules.len();
    let onclick = props.onclick.clone().unwrap_or_default();
    html! {
        <button
            type="button"
            class="tp__filter-summary"
            onclick={onclick}
            disabled={props.readonly || props.onclick.is_none()}
            aria-label={translate.t("FILTER_BUILDER.OPEN")}
        >
            <span class={classes!("tp__filter-summary__stage", (count == 0).then_some("empty"))}>
                <strong>{ translate.t("FILTER_BUILDER.MATCHING_ITEMS_REMOVED") }</strong>
                { if count > 0 { translate.t("FILTER_BUILDER.REMOVAL_COUNT").replace("{count}", &count.to_string()) }
                    else { translate.t("FILTER_BUILDER.NO_REMOVALS") } }
            </span>
            if has_advanced || processing.is_some() {
                <span class="tp__filter-summary__stage">
                    <strong>{ translate.t("FILTER_BUILDER.OPTIONAL_ADVANCED") }</strong>
                    { processing.unwrap_or_else(|| translate.t("FILTER_BUILDER.LEGACY_KEEP")) }
                </span>
            }
        </button>
    }
}

fn import_preview(
    expression: Option<&str>,
    rules: &[ManagedFilterRule],
    templates: &[PatternTemplate],
    translate: &crate::i18n::YewI18n,
) -> Option<String> {
    let expression = expression.map(str::trim).filter(|value| !value.is_empty())?;
    let node = shared::foundation::import_filter_draft_with_templates(Some(expression), rules, Some(templates))?;
    Some(match node {
        FilterNode::Reference { rule_id } => {
            rules.iter().find(|rule| rule.id == rule_id).map(|rule| rule.name.clone())?
        }
        FilterNode::LegacyReference { template } => {
            format!("{}: !{}!", translate.t("FILTER_BUILDER.TEMPLATE"), template)
        }
        FilterNode::Raw { .. } => translate.t("FILTER_BUILDER.ADVANCED"),
        FilterNode::Group { ref children, .. } => {
            format!("{} {}", children.len(), translate.t("FILTER_BUILDER.BLOCKS"))
        }
        _ => format!("1 {}", translate.t("FILTER_BUILDER.BLOCK")),
    })
}
