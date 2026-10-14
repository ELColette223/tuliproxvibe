use crate::{
    app::components::{AppIcon, LoadingIndicator, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    services::TargetBouquetService,
};
use shared::{
    foundation::{compile_visual_rules_to_filter_dsl, VisualCategoryRule},
    model::{
        permission::Permission, ConfigTargetFilterDto, PlaylistClusterBouquetDto,
        TargetBouquetCategorySelectionDto, TargetBouquetDto, TargetBouquetMode, TargetBouquetSelectionDto,
        TargetBouquetStreamEventDto, XtreamCluster,
    },
};
use std::collections::{HashMap, HashSet};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CategoryKey {
    cluster: XtreamCluster,
    name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Rule {
    All,
    None,
    Partial(HashSet<String>),
}

impl Rule {
    fn contains(&self, id: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Partial(ids) => ids.contains(id),
        }
    }

    fn toggle_channel(&mut self, id: &str, all_loaded_ids: &HashSet<String>) {
        let mut ids = match self {
            Self::All => all_loaded_ids.clone(),
            Self::None => HashSet::new(),
            Self::Partial(ids) => ids.clone(),
        };
        if !ids.insert(id.to_owned()) {
            ids.remove(id);
        }
        *self = if ids.is_empty() { Self::None } else { Self::Partial(ids) };
    }

    fn label(&self, loaded: Option<&Inventory>) -> &'static str {
        match self {
            Self::All => "All",
            Self::None => "None",
            Self::Partial(ids) if ids.is_empty() => "None",
            Self::Partial(ids) if loaded.is_some_and(|page| page.complete && page.ids() == *ids) => "All",
            Self::Partial(_) => "Partial",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Inventory {
    channels: Vec<Channel>,
    complete: bool,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Channel {
    id: String,
    name: String,
    input: String,
}

impl Inventory {
    fn ids(&self) -> HashSet<String> { self.channels.iter().map(|channel| channel.id.clone()).collect() }
}

fn save_apply_feedback(applied: bool) -> (&'static str, bool) {
    if applied { ("FILTER_BUILDER.SAVED_APPLIED_SUCCESS", false) } else { ("FILTER_BUILDER.SAVED_NEXT_UPDATE", true) }
}

fn legacy_rule(bouquet: Option<&TargetBouquetDto>, key: &CategoryKey) -> Rule {
    let Some(bouquet) = bouquet else { return Rule::All };
    let groups = match key.cluster {
        XtreamCluster::Live => &bouquet.groups.live,
        XtreamCluster::Video => &bouquet.groups.vod,
        XtreamCluster::Series => &bouquet.groups.series,
    };
    let listed = groups.as_ref().is_some_and(|groups| groups.contains(&key.name));
    match bouquet.mode {
        TargetBouquetMode::Whitelist if groups.is_none() || listed => Rule::All,
        TargetBouquetMode::Blacklist if !listed => Rule::All,
        _ => Rule::None,
    }
}

fn initial_rules(categories: &[CategoryKey], bouquet: Option<&TargetBouquetDto>) -> HashMap<CategoryKey, Rule> {
    categories.iter().map(|key| {
        let rule = bouquet.and_then(|value| value.selection.as_ref()).map_or_else(
            || legacy_rule(bouquet, key),
            |selection| selection.categories.iter().find(|category| category.cluster == key.cluster && category.name == key.name)
                .map_or(Rule::All, |category| {
                    if category.included_channel_ids.is_empty() { Rule::None }
                    else { Rule::Partial(category.included_channel_ids.iter().cloned().collect()) }
                }),
        );
        (key.clone(), rule)
    }).collect()
}

/// Only serialize known categories after their complete inventory has been fetched. An absent
/// category in the wire format means *new category*, never "all existing channels".
fn snapshot(
    categories: &[CategoryKey],
    rules: &HashMap<CategoryKey, Rule>,
    inventories: &HashMap<CategoryKey, Inventory>,
    missing: &HashSet<CategoryKey>,
    previous: Option<&TargetBouquetDto>,
) -> Result<TargetBouquetSelectionDto, String> {
    let mut result = Vec::with_capacity(categories.len());
    for key in categories {
        if missing.contains(key) {
            let saved = previous.and_then(|bouquet| bouquet.selection.as_ref())
                .and_then(|selection| selection.categories.iter().find(|category| {
                    category.cluster == key.cluster && category.name == key.name
                })).ok_or_else(|| format!("Missing saved category {} · {}", key.cluster, key.name))?;
            let mut retained = saved.clone();
            retained.included_channel_ids = match rules.get(key) {
                Some(Rule::None) => Vec::new(),
                Some(Rule::Partial(ids)) => ids.iter().cloned().collect(),
                _ => saved.included_channel_ids.clone(),
            };
            result.push(retained);
            continue;
        }
        let inventory = inventories.get(key).filter(|inventory| inventory.complete && inventory.error.is_none())
            .ok_or_else(|| format!("Load every channel in {} · {} before saving", key.cluster, key.name))?;
        let mut ids: Vec<String> = inventory.channels.iter()
            .filter(|channel| rules.get(key).unwrap_or(&Rule::All).contains(&channel.id))
            .map(|channel| channel.id.clone()).collect();
    // Selected IDs absent from the current complete inventory remain in the saved selection.

        if let Some(Rule::Partial(selected)) = rules.get(key) {
            ids.extend(selected.iter().cloned());
        }
        ids.sort_unstable();
        ids.dedup();
        result.push(TargetBouquetCategorySelectionDto {
            cluster: key.cluster, name: key.name.clone(), included_channel_ids: ids,
        });
    }
    Ok(TargetBouquetSelectionDto { categories: result, base_filter: None })
}

#[derive(Properties, PartialEq)]
pub struct VisualTargetFilterProps {
    pub target_name: String,
    pub on_back: Callback<()>,
}

#[component]
pub fn VisualTargetFilter(props: &VisualTargetFilterProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let can_write = services.auth.has_permission(Permission::PlaylistWrite);
    let categories = use_state(Vec::<CategoryKey>::new);
    let missing_categories = use_state(HashSet::<CategoryKey>::new);
    let rules = use_state(HashMap::<CategoryKey, Rule>::new);
    let inventory = use_state(HashMap::<CategoryKey, Inventory>::new);
    let inventory_cache = use_mut_ref(HashMap::<CategoryKey, Inventory>::new);
    let expanded = use_state(HashSet::<CategoryKey>::new);
    let fetching = use_state(HashSet::<CategoryKey>::new);
    let bouquet = use_state(|| None::<TargetBouquetDto>);
    let warnings = use_state(Vec::<String>::new);
    let load_failed = use_state(|| false);
    let loading = use_state(|| true);
    let saving = use_state(|| false);
    let dirty = use_state(|| false);
    let query = use_state(String::new);
    let revision = use_state(|| 0_u64);
    let generation = use_mut_ref(|| 0_u64);
    let save_progress = use_state(|| None::<String>);

    {
        let target = props.target_name.clone();
        let revision_value = *revision;
        let categories = categories.clone();
        let missing_categories = missing_categories.clone();
        let rules = rules.clone();
        let inventory = inventory.clone();
        let inventory_cache = inventory_cache.clone();
        let expanded = expanded.clone();
        let fetching = fetching.clone();
        let bouquet = bouquet.clone();
        let warnings = warnings.clone();
        let load_failed = load_failed.clone();
        let loading = loading.clone();
        let dirty = dirty.clone();
        let generation = generation.clone();
        use_effect_with((target, revision_value), move |(target, _)| {
            let current = { let mut generation = generation.borrow_mut(); *generation += 1; *generation };
            let controller = web_sys::AbortController::new().ok();
            let signal = controller.as_ref().map(web_sys::AbortController::signal);
            let target = target.clone();
            categories.set(Vec::new());
            missing_categories.set(HashSet::new());
            rules.set(HashMap::new());
            inventory_cache.borrow_mut().clear();
            inventory.set(HashMap::new());
            expanded.set(HashSet::new());
            fetching.set(HashSet::new());
            bouquet.set(None);
            warnings.set(Vec::new());
            load_failed.set(false);
            dirty.set(false);
            loading.set(true);
            spawn_local(async move {
                let mut found = HashSet::new();
                let mut selected = None;
                let mut issues = Vec::new();
                let result = TargetBouquetService::fetch_target_bouquet_stream(&target, signal, |event| match event {
                    TargetBouquetStreamEventDto::Selection { bouquet } => selected = bouquet,
                    TargetBouquetStreamEventDto::InputChunk { cluster, groups, .. } => {
                        found.extend(groups.into_iter().map(|name| CategoryKey { cluster, name }));
                    }
                    TargetBouquetStreamEventDto::InputWarning { message, .. } => issues.push(message),
                    _ => {}
                }).await;
                if *generation.borrow() != current { return; }
                if let Err(error) = result {
                    issues.push(format!("Failed to load bouquet: {error}"));
                    load_failed.set(true);
                }
                // Saved categories absent from the stream retain their exact stored IDs.
                let mut absent = HashSet::new();
                if let Some(selection) = selected.as_ref().and_then(|bouquet: &TargetBouquetDto| bouquet.selection.as_ref()) {
                    for item in &selection.categories {
                        let key = CategoryKey { cluster: item.cluster, name: item.name.clone() };
                        if found.insert(key.clone()) { absent.insert(key); }
                    }
                }
                missing_categories.set(absent);
                let mut ordered: Vec<CategoryKey> = found.into_iter().collect();
                ordered.sort_by(|a, b| (a.cluster as u8, &a.name).cmp(&(b.cluster as u8, &b.name)));
                rules.set(initial_rules(&ordered, selected.as_ref()));
                bouquet.set(selected);
                categories.set(ordered);
                warnings.set(issues);
                loading.set(false);
            });
            move || { if let Some(controller) = controller { controller.abort(); } }
        });
    }

    let load_inventory = {
        let inventory = inventory.clone();
        let inventory_cache = inventory_cache.clone();
        let fetching = fetching.clone();
        let missing_categories = missing_categories.clone();
        let generation = generation.clone();
        let target = props.target_name.clone();
        Callback::from(move |key: CategoryKey| {
            if missing_categories.contains(&key) || inventory.get(&key).is_some_and(|entry| entry.complete) || fetching.contains(&key) { return; }
            let mut busy = (*fetching).clone();
            busy.insert(key.clone());
            fetching.set(busy);
            let current = *generation.borrow();
            let target = target.clone();
            let inventory = inventory.clone();
            let inventory_cache = inventory_cache.clone();
            let fetching = fetching.clone();
            let generation = generation.clone();
            spawn_local(async move {
                let mut channels = Vec::<Channel>::new();
                let mut seen = HashSet::<String>::new();
                let mut visited = HashSet::<String>::new();
                let mut cursor: Option<String> = None;
                let outcome = loop {
                    let page = TargetBouquetService::fetch_target_channel_inventory(
                        &target, key.cluster, &key.name, cursor.clone(),
                    ).await;
                    if *generation.borrow() != current { return; }
                    match page {
                        Err(error) => break Err(format!("Could not load channels: {error}")),
                        Ok(page) => {
                            if !page.unavailable_inputs.is_empty() || page.status == "unavailable" {
                                break Err(format!("Inventory unavailable{}", if page.unavailable_inputs.is_empty() {
                                    String::new()
                                } else {
                                    format!(": {}", page.unavailable_inputs.join(", "))
                                }));
                            }
                            for channel in page.channels {
                                if seen.insert(channel.id.clone()) {
                                    channels.push(Channel { id: channel.id, name: channel.name, input: channel.input });
                                }
                            }
                            match page.next_cursor {
                                Some(next) if visited.insert(next.clone()) => cursor = Some(next),
                                Some(_) => break Err("Channel pagination repeated a cursor".to_owned()),
                                None => break Ok(()),
                            }
                        }
                    }
                };
                channels.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
                let entry = Inventory { channels, complete: outcome.is_ok(), error: outcome.err() };
                inventory_cache.borrow_mut().insert(key.clone(), entry);
                inventory.set(inventory_cache.borrow().clone());
                let mut busy = (*fetching).clone();
                busy.remove(&key);
                fetching.set(busy);
            });
        })
    };

    let on_save = {
        let target = props.target_name.clone();
        let categories = categories.clone();
        let missing_categories = missing_categories.clone();
        let rules = rules.clone();
        let inventory_cache = inventory_cache.clone();
        let bouquet = bouquet.clone();
        let dirty = dirty.clone();
        let warnings = warnings.clone();
        let saving = saving.clone();
        let save_progress = save_progress.clone();
        let revision = revision.clone();
        let services = services.clone();
        let generation = generation.clone();
        let translate = translate.clone();
        Callback::from(move |_| {
            if *saving || !can_write { return; }
            let old = (*bouquet).clone();
            let is_legacy = old.as_ref().is_some_and(|value| value.selection.is_none());
            if is_legacy && !web_sys::window().is_some_and(|window| window.confirm_with_message(
                &translate.t("FILTER_BUILDER.LEGACY_CONFIRM")
            ).unwrap_or(false)) { return; }
            let current = *generation.borrow();
            let target = target.clone();
            let categories = (*categories).clone();
            let missing_categories = (*missing_categories).clone();
            let rules = (*rules).clone();
            let inventory_cache = inventory_cache.clone();
            let saving = saving.clone();
            let save_progress = save_progress.clone();
            let warnings = warnings.clone();
            let bouquet = bouquet.clone();
            let dirty = dirty.clone();
            let revision = revision.clone();
            let services = services.clone();
            let generation = generation.clone();
            let translate = translate.clone();
            saving.set(true);
            spawn_local(async move {
                let fail = |save_progress: &UseStateHandle<Option<String>>, saving: &UseStateHandle<bool>, msg: String| {
                    save_progress.set(None);
                    saving.set(false);
                    msg
                };
                // Finish the entire inventory before building a snapshot. Never treat an
                // unloaded category or an unavailable input as an empty selection.
                // ONE pass over the raw inputs via the streaming inventory endpoint
                // replaces the old per-category sequential scans.
                save_progress.set(Some("inventory".to_string()));
                let mut inventory = inventory_cache.borrow().clone();
                let mut stream_fail: Option<String> = None;
                let stream_result = TargetBouquetService::fetch_target_inventory_stream(&target, None, |event| {
                    match event {
                        TargetBouquetStreamEventDto::InventoryChunk { input, cluster, group, channels } => {
                            let key = CategoryKey { cluster, name: group };
                            let entry = inventory.entry(key).or_insert_with(Inventory::default);
                            for channel in channels {
                                // Same channel may be served by several inputs; keep first.
                                if entry.channels.iter().all(|known| known.id != channel.id) {
                                    entry.channels.push(Channel {
                                        id: channel.id,
                                        name: channel.name,
                                        input: input.clone(),
                                    });
                                }
                            }
                        }
                        TargetBouquetStreamEventDto::InputWarning { message, .. } => {
                            if stream_fail.is_none() { stream_fail = Some(message); }
                        }
                        _ => {}
                    }
                })
                .await;
                if *generation.borrow() != current { save_progress.set(None); return; }
                let inventory_pass = match stream_result {
                    Ok(()) if stream_fail.is_none() => Ok(()),
                    Ok(()) => Err(stream_fail.expect("checked above")),
                    Err(error) => Err(format!("Could not load channel inventory: {error}")),
                };
                if let Err(message) = inventory_pass {
                    let msg = fail(&save_progress, &saving, message);
                    warnings.set(vec![msg.clone()]);
                    services.toastr.error(msg);
                    return;
                }
                // Every current category now has a complete inventory.
                for key in &categories {
                    inventory.entry(key.clone()).or_default().complete = true;
                }
                inventory_cache.borrow_mut().extend(inventory.clone());
                if *generation.borrow() != current { save_progress.set(None); return; }
                inventory.extend(inventory_cache.borrow().clone());
                let selection = match snapshot(&categories, &rules, &inventory, &missing_categories, old.as_ref()) {
                    Ok(selection) if selection.categories.iter().any(|category| !category.included_channel_ids.is_empty()) => selection,
                    Ok(_) => {
                        let msg = fail(&save_progress, &saving, translate.t("FILTER_BUILDER.SELECTION_REQUIRED"));
                        warnings.set(vec![msg.clone()]);
                        services.toastr.error(msg);
                        return;
                    }
                    Err(message) => {
                        let msg = fail(&save_progress, &saving, message);
                        warnings.set(vec![msg.clone()]);
                        services.toastr.error(msg);
                        return;
                    }
                };

                // 1. Compile the visual selection into a persistent precise regex/DSL filter
                //    and persist it into source.yml FIRST. Every failure here is shown to the
                //    user and aborts the save instead of being swallowed.
                save_progress.set(Some("filter".to_string()));
                let mut category_rules = Vec::with_capacity(categories.len());
                for key in &categories {
                    let rule = rules.get(key).unwrap_or(&Rule::All);
                    let is_enabled = rule != &Rule::None;
                    let kept_channel_names: Vec<String> = inventory.get(key).map_or_else(Vec::new, |inv| {
                        inv.channels.iter().filter(|ch| rule.contains(&ch.id)).map(|ch| ch.name.clone()).collect()
                    });
                    category_rules.push(VisualCategoryRule {
                        name: key.name.clone(),
                        is_enabled,
                        kept_channel_names,
                    });
                }
                // The base filter is the NON-visual filter the user had (kept in the
                // bouquet since the first visual save). Using the live target filter
                // here would nest the previous visual compilation on every save.
                let (server_config, _) = services.config.get_server_config().await;
                let Some(cfg) = server_config else {
                    let msg = fail(&save_progress, &saving, "Could not load current configuration to persist the filter".to_string());
                    services.toastr.error(msg);
                    return;
                };
                let saved_base = old.as_ref().and_then(|bouquet| bouquet.selection.as_ref()).and_then(|selection| selection.base_filter.clone());
                let mut sources = cfg.sources.clone();
                let base_filter = saved_base.or_else(|| {
                    // First visual save: capture the current (hand-written) filter as
                    // the base so future rebuilds replace, not nest, the visual part.
                    sources.sources.iter().flat_map(|source| &source.targets)
                        .find(|candidate| candidate.name == target)
                        .and_then(|candidate| candidate.filter.processing.as_deref().or(candidate.filter.persist.as_deref()))
                        .map(str::to_string)
                });
                let mut compiled_dsl: Option<String> = None;
                for source in &mut sources.sources {
                    for candidate in &mut source.targets {
                        if candidate.name == target {
                            match compile_visual_rules_to_filter_dsl(base_filter.as_deref(), &category_rules) {
                                Ok(Some(dsl)) => {
                                    candidate.filter = ConfigTargetFilterDto::from(dsl.clone());
                                    compiled_dsl = Some(dsl);
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    let msg = fail(&save_progress, &saving, format!("Failed to compile filter: {err}"));
                                    services.toastr.error(msg);
                                    return;
                                }
                            }
                        }
                    }
                }
                if compiled_dsl.is_some() {
                    if let Err(e) = services.config.save_sources(sources).await {
                        let msg = fail(&save_progress, &saving, format!("Failed to persist filter into source.yml: {e}"));
                        warnings.set(vec![msg.clone()]);
                        services.toastr.error(msg);
                        return;
                    }
                }

                // 2. Persist the visual selection (bouquet snapshot) for the editor,
                //    remembering the non-visual base filter for future rebuilds.
                save_progress.set(Some("selection".to_string()));
                let mut selection = selection;
                selection.base_filter = base_filter;
                let base = old.unwrap_or_else(|| TargetBouquetDto::new(
                    TargetBouquetMode::Whitelist, PlaylistClusterBouquetDto::default(),
                ));
                let updated = TargetBouquetDto::new(base.mode, base.groups).with_selection(selection);
                if let Err(error) = TargetBouquetService::save_target_bouquet(&target, &updated).await {
                    let msg = fail(&save_progress, &saving, format!("Failed to save visual target filter: {error}"));
                    services.toastr.error(msg);
                    return;
                }

                // The selection is persisted even if immediate application fails. Keep it
                // visible in the editor rather than resetting to an unsaved-looking state.
                bouquet.set(Some(updated));
                dirty.set(false);

                // 3. Immediately re-materialize the target playlist from the local input cache.
                save_progress.set(Some("apply".to_string()));
                let apply_result = services.playlist.reprocess_target(&target).await;
                let (message_key, is_warning) = save_apply_feedback(apply_result.is_ok());
                if let Err(error) = apply_result {
                    let message = format!("{} {error}", translate.t(message_key));
                    warnings.set(vec![message.clone()]);
                    services.toastr.warning(message);
                } else if !is_warning {
                    services.toastr.success(translate.t(message_key));
                    revision.set(revision.wrapping_add(1));
                }

                save_progress.set(None);
                saving.set(false);
            });
        })
    };

    let on_search = {
        let query = query.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event.target().and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok()) {
                query.set(input.value());
            }
        })
    };
    let search = query.to_lowercase();
    let legacy = bouquet.as_ref().is_some_and(|value| value.selection.is_none());
    let tr = {
        let translate = translate.clone();
        move |key: &str, default_text: &'static str| {
            let s = translate.t(key);
            if s.starts_with("Key '") { default_text.to_string() } else { s }
        }
    };
    html! {
        <section class="tp__visual-target-filter" aria-label="Visual target filter">
            <header class="tp__visual-target-filter__header">
                <div class="tp__visual-target-filter__heading">
                    <TextButton name="Back" title={tr("LABEL.BACK", "Back")} icon="Back" class="tertiary" onclick={props.on_back.reform(|_: String| ())} />
                    <h2>{ format!("{} · {}", tr("FILTER_BUILDER.VISUAL_TITLE", "Choose what stays in this target"), props.target_name) }</h2>
                </div>
                <div class="tp__visual-target-filter__actions">
                    <button type="button" onclick={{ let revision = revision.clone(); Callback::from(move |_| revision.set(revision.wrapping_add(1))) }} disabled={*loading || *saving || *dirty}>{ tr("FILTER_BUILDER.REFRESH_INVENTORY", "Refresh channels") }</button>
                    <button type="button" class="tp__visual-target-filter__save" onclick={on_save}
                        disabled={!can_write || *loading || *saving || (!*dirty && !legacy) || *load_failed}>
                        <AppIcon name="Save" width="18px" height="18px" />
                        { if *saving {
                            let progress = save_progress.as_ref().map(|p| format!(" {p}")).unwrap_or_default();
                            html! { <>
                                { format!("{}{}", translate.t("LABEL.SAVE"), progress) }
                            </> }
                        } else { html! { tr("FILTER_BUILDER.SAVE_SELECTION", "Save selection") } } }
                    </button>
                </div>
            </header>
            <p class="tp__visual-target-filter__notice">
                { tr("FILTER_BUILDER.VISUAL_DESCRIPTION", "Choose categories and channels to keep.") }
                { " " }{ tr("FILTER_BUILDER.NEW_CATEGORY_AUTOMATIC", "New categories are included automatically after a source update.") }
                { " " }{ tr("FILTER_BUILDER.NEW_CHANNEL_MANUAL", "New channels in existing categories must be included manually.") }
            </p>
            { if legacy { html! { <p class="tp__visual-target-filter__legacy" role="status">
                { tr("FILTER_BUILDER.LEGACY_CATEGORY_SELECTION", "This target uses the previous category selection. It will remain unchanged until you save a new selection.") }
            </p> } } else { html! {} } }
            { for warnings.iter().map(|warning| html! { <p class="tp__visual-target-filter__error" role="alert">{ warning }</p> }) }
            <label class="tp__visual-target-filter__search">
                <AppIcon name="Search" width="20px" height="20px" />
                <span class="tp__visual-target-filter__sr-only">{ tr("FILTER_BUILDER.SEARCH_CATEGORIES", "Search categories") }</span>
                <input type="search" aria-label={tr("FILTER_BUILDER.SEARCH_CATEGORIES", "Search categories")} placeholder={tr("FILTER_BUILDER.SEARCH_CATEGORIES", "Search categories")} value={(*query).clone()} oninput={on_search} />
            </label>
            { if *loading { html! { <LoadingIndicator loading={true} /> } } else { html! {
                <div class="tp__visual-target-filter__list">
                { for categories.iter().filter(|key| {
                    search.is_empty() || key.name.to_lowercase().contains(&search) ||
                    inventory.get(*key).is_some_and(|page| page.channels.iter().any(|channel| channel.name.to_lowercase().contains(&search)))
                }).map(|key| {
                    let rule = rules.get(key).cloned().unwrap_or(Rule::All);
                    let page = inventory.get(key);
                    let status = rule.label(page);
                    let is_missing = missing_categories.contains(key);
                    let status_text = match status {
                        "All" => tr("FILTER_BUILDER.CATEGORY_INCLUDED", "Category included"),
                        "None" => tr("FILTER_BUILDER.CATEGORY_EXCLUDED", "Category excluded"),
                        _ => tr("FILTER_BUILDER.CATEGORY_PARTIAL", "Some channels included"),
                    };
                    let selected_count = page.filter(|page| page.complete).map(|page| page.channels.iter().filter(|channel| rule.contains(&channel.id)).count());
                    let is_open = expanded.contains(key);
                    let is_fetching = fetching.contains(key);
                    let key_toggle = key.clone();
                    let rules_for_toggle = rules.clone();
                    let dirty_for_toggle = dirty.clone();
                    let toggle = Callback::from(move |_| {
                        let mut next = (*rules_for_toggle).clone();
                        next.insert(key_toggle.clone(), if next.get(&key_toggle) == Some(&Rule::None) { Rule::All } else { Rule::None });
                        rules_for_toggle.set(next);
                        dirty_for_toggle.set(true);
                    });
                    let key_expand = key.clone();
                    let expanded_for_click = expanded.clone();
                    let load = load_inventory.clone();
                    let expand = Callback::from(move |_| {
                        let mut next = (*expanded_for_click).clone();
                        if !next.remove(&key_expand) {
                            next.insert(key_expand.clone());
                            load.emit(key_expand.clone());
                        }
                        expanded_for_click.set(next);
                    });
                    html! {
                        <article key={format!("{}:{}", key.cluster, key.name)} class="tp__visual-target-filter__category">
                            <div class="tp__visual-target-filter__category-head">
                                <button type="button" class={classes!("tp__visual-target-filter__toggle", (status != "None").then_some("is-on"))}
                                    aria-label={format!("{}: {} · {}", if status == "None" {
                                        tr("FILTER_BUILDER.INCLUDE_CATEGORY", "Include category")
                                    } else { tr("FILTER_BUILDER.EXCLUDE_CATEGORY", "Exclude category") }, key.cluster, key.name)}
                                    aria-pressed={(status != "None").to_string()} disabled={!can_write || *saving} onclick={toggle}>
                                    <AppIcon name={if status == "None" { "Unchecked" } else { "Checked" }} width="20px" height="20px" />
                                </button>
                                <button type="button" class="tp__visual-target-filter__expand" aria-expanded={is_open.to_string()}
                                    aria-label={format!("{}: {} · {}", if is_open {
                                        tr("FILTER_BUILDER.HIDE_CHANNELS", "Hide channels")
                                    } else { tr("FILTER_BUILDER.SHOW_CHANNELS", "Show channels") }, key.cluster, key.name)} onclick={expand}>
                                    <span class="tp__visual-target-filter__cluster">{ key.cluster.to_string() }</span>
                                    <span class="tp__visual-target-filter__name">{ &key.name }</span>
                                    <span class="tp__visual-target-filter__state">
                                        { status_text }
                                        { selected_count.map(|count| format!(" · {count}/{}", page.map_or(0, |page| page.channels.len()))).unwrap_or_default() }
                                    </span>
                                    <AppIcon name={if is_open { "ExpandLess" } else { "ExpandMore" }} width="20px" height="20px" />
                                </button>
                            </div>
                            { if is_open { html! {
                                <div class="tp__visual-target-filter__channels">
                                    { if is_fetching { html! { <p role="status">{ tr("LABEL.LOADING", "Loading…") }</p> } } else { html! {} } }
                                    { if is_missing { html! { <p role="status">{ tr("FILTER_BUILDER.INVENTORY_UNAVAILABLE", "Channel inventory is unavailable.") }</p> } } else { html! {} } }
                                    { if let Some(error) = page.and_then(|page| page.error.as_ref()) { html! {
                                        <p class="tp__visual-target-filter__error" role="alert">{ error }</p>
                                    } } else { html! {} } }
                                    { for page.into_iter().flat_map(|page| page.channels.iter()).filter(|channel| {
                                        search.is_empty() || key.name.to_lowercase().contains(&search) || channel.name.to_lowercase().contains(&search)
                                    }).map(|channel| {
                                        let id = channel.id.clone();
                                        let item_key = key.clone();
                                        let rules_for_channel = rules.clone();
                                        let inventory_for_channel = inventory.clone();
                                        let dirty_for_channel = dirty.clone();
                                        let on_change = Callback::from(move |_| {
                                            let Some(page) = inventory_for_channel.get(&item_key).filter(|page| page.complete) else { return };
                                            let mut next = (*rules_for_channel).clone();
                                            next.entry(item_key.clone()).or_insert(Rule::All).toggle_channel(&id, &page.ids());
                                            rules_for_channel.set(next);
                                            dirty_for_channel.set(true);
                                        });
                                        html! {
                                            <label key={channel.id.clone()} class="tp__visual-target-filter__channel">
                                                <input type="checkbox" checked={rule.contains(&channel.id)} disabled={!can_write || *saving || !page.is_some_and(|page| page.complete)} onchange={on_change}
                                                    aria-label={format!("{}: {} · {}", if rule.contains(&channel.id) {
                                                        tr("FILTER_BUILDER.EXCLUDE_CHANNEL", "Exclude channel")
                                                    } else { tr("FILTER_BUILDER.INCLUDE_CHANNEL", "Include channel") }, channel.name, channel.input)} />
                                                <span class="tp__visual-target-filter__channel-name">{ &channel.name }</span>
                                                <small>{ &channel.input }</small>
                                            </label>
                                        }
                                    }) }
                                    { if page.is_some_and(|page| page.complete && page.channels.is_empty()) { html! { <p>{ tr("FILTER_BUILDER.NO_CHANNELS", "No channels found") }</p> } } else { html! {} } }
                                </div>
                            } } else { html! {} } }
                        </article>
                    }
                }) }
                { if categories.is_empty() { html! { <p>{ tr("FILTER_BUILDER.NO_CATEGORIES", "No categories found") }</p> } } else { html! {} } }
                </div>
            } } }
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> CategoryKey { CategoryKey { cluster: XtreamCluster::Live, name: name.into() } }

    #[test]
    fn saved_selection_reports_apply_failure_as_warning_not_success() {
        assert_eq!(save_apply_feedback(false), ("FILTER_BUILDER.SAVED_NEXT_UPDATE", true));
        assert_eq!(save_apply_feedback(true), ("FILTER_BUILDER.SAVED_APPLIED_SUCCESS", false));
    }
    fn loaded(ids: &[&str]) -> Inventory {
        Inventory { channels: ids.iter().map(|id| Channel { id: (*id).into(), name: (*id).into(), input: "source".into() }).collect(), complete: true, error: None }
    }

    #[test]
    fn all_and_none_and_partial_are_explicit() {
        let keys = vec![key("News"), key("Sports"), key("Kids")];
        let inventory = HashMap::from([
            (keys[0].clone(), loaded(&["a", "b"])),
            (keys[1].clone(), loaded(&["c"])),
            (keys[2].clone(), loaded(&["d", "e"])),
        ]);
        let rules = HashMap::from([
            (keys[0].clone(), Rule::All), (keys[1].clone(), Rule::None),
            (keys[2].clone(), Rule::Partial(HashSet::from(["e".into()]))),
        ]);
        let selection = snapshot(&keys, &rules, &inventory, &HashSet::new(), None).unwrap();
        assert_eq!(selection.categories[0].included_channel_ids, vec!["a", "b"]);
        assert!(selection.categories[1].included_channel_ids.is_empty());
        assert_eq!(selection.categories[2].included_channel_ids, vec!["e"]);
        assert!(!selection.allows(XtreamCluster::Live, "News", "future"));
        assert!(selection.allows(XtreamCluster::Live, "Brand new", "future"));
    }

    #[test]
    fn unloaded_inventory_must_not_turn_all_into_empty() {
        assert!(snapshot(&[key("News")], &HashMap::new(), &HashMap::new(), &HashSet::new(), None).is_err());
    }

    #[test]
    fn missing_saved_category_retains_ids_and_allows_explicit_disable() {
        let old = TargetBouquetDto::new(TargetBouquetMode::Whitelist, PlaylistClusterBouquetDto::default())
            .with_selection(TargetBouquetSelectionDto {
                categories: vec![TargetBouquetCategorySelectionDto {
                    cluster: XtreamCluster::Live, name: "Removed".into(),
                    included_channel_ids: vec!["saved-id".into()],
                }],
                base_filter: None,
            });
        let categories = vec![key("Removed")];
        let missing = HashSet::from([key("Removed")]);
        let rules = initial_rules(&categories, Some(&old));
        let saved = snapshot(&categories, &rules, &HashMap::new(), &missing, Some(&old)).unwrap();
        assert_eq!(saved.categories[0].included_channel_ids, vec!["saved-id"]);
        let disabled = HashMap::from([(key("Removed"), Rule::None)]);
        let saved = snapshot(&categories, &disabled, &HashMap::new(), &missing, Some(&old)).unwrap();
        assert!(saved.categories[0].included_channel_ids.is_empty());
    }

    #[test]
    fn channel_toggle_from_all_preserves_other_loaded_ids() {
        let mut rule = Rule::All;
        rule.toggle_channel("a", &HashSet::from(["a".into(), "b".into()]));
        assert_eq!(rule, Rule::Partial(HashSet::from(["b".into()])));
        rule.toggle_channel("b", &HashSet::from(["a".into(), "b".into()]));
        assert_eq!(rule, Rule::None);
    }

    #[test]
    fn legacy_blacklist_remains_visible_until_explicit_save() {
        let old = TargetBouquetDto::new(TargetBouquetMode::Blacklist, PlaylistClusterBouquetDto {
            live: Some(vec!["News".into()]), vod: None, series: None,
        });
        assert_eq!(legacy_rule(Some(&old), &key("News")), Rule::None);
        assert_eq!(legacy_rule(Some(&old), &key("Sports")), Rule::All);
        assert!(old.selection.is_none());
    }
}
