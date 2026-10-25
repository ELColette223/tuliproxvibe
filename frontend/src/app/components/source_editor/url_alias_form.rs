use crate::{
    app::components::{input::Input, Card, IconButton, TextButton},
    i18n::use_translation,
};
use shared::utils::Internable;
use std::sync::Arc;
use yew::prelude::*;

const LABEL_URL_ALIASES: &str = "LABEL.URL_ALIASES";
const LABEL_URL_ALIAS: &str = "LABEL.URL_ALIAS";
const LABEL_ADD_URL_ALIAS: &str = "LABEL.ADD_URL_ALIAS";

#[derive(Properties, PartialEq, Clone)]
pub struct UrlAliasListProps {
    /// Provider base URLs in preference order; the order is the failover order.
    pub urls: Vec<Arc<str>>,
    pub on_change: Callback<Vec<Arc<str>>>,
    #[prop_or(false)]
    pub readonly: bool,
}

fn emit(urls: Vec<Arc<str>>, on_change: &Callback<Vec<Arc<str>>>) {
    let cleaned: Vec<Arc<str>> =
        urls.into_iter().map(|url| url.trim().intern()).filter(|url: &Arc<str>| !url.is_empty()).collect();
    on_change.emit(cleaned);
}

/// Editor for the input's alternative provider base URLs (laterals).
///
/// Uses the same form-list, input, icon-button and text-button components as the other source
/// editor pages. That keeps spacing, focus, hover, disabled state and icon rendering consistent
/// with the rest of Tuliprox.
#[component]
pub fn UrlAliasList(props: &UrlAliasListProps) -> Html {
    let translate = use_translation();

    let on_edit = {
        let urls = props.urls.clone();
        let on_change = props.on_change.clone();
        move |index: usize, value: String| {
            let mut next = urls.clone();
            if let Some(entry) = next.get_mut(index) {
                *entry = value.intern();
            }
            emit(next, &on_change);
        }
    };

    let on_remove = {
        let urls = props.urls.clone();
        let on_change = props.on_change.clone();
        Callback::from(move |(name, _event): (String, MouseEvent)| {
            let Ok(index) = name.parse::<usize>() else { return };
            let mut next = urls.clone();
            if index < next.len() {
                next.remove(index);
                emit(next, &on_change);
            }
        })
    };

    let on_move_up = {
        let urls = props.urls.clone();
        let on_change = props.on_change.clone();
        Callback::from(move |(name, _event): (String, MouseEvent)| {
            let Ok(index) = name.parse::<usize>() else { return };
            if index == 0 || index >= urls.len() {
                return;
            }
            let mut next = urls.clone();
            next.swap(index, index - 1);
            emit(next, &on_change);
        })
    };

    let on_move_down = {
        let urls = props.urls.clone();
        let on_change = props.on_change.clone();
        Callback::from(move |(name, _event): (String, MouseEvent)| {
            let Ok(index) = name.parse::<usize>() else { return };
            if index + 1 >= urls.len() {
                return;
            }
            let mut next = urls.clone();
            next.swap(index, index + 1);
            emit(next, &on_change);
        })
    };

    let on_add = {
        let urls = props.urls.clone();
        let on_change = props.on_change.clone();
        Callback::from(move |_: String| {
            let mut next = urls.clone();
            next.push("".intern());
            on_change.emit(next);
        })
    };

    html! {
        <Card class="tp__config-view__card tp__item-form">
            <div class="tp__form-field">
                <label class="tp__form-field__label">{ translate.t(LABEL_URL_ALIASES) }</label>
            </div>
            <div class="tp__form-list">
                <div class="tp__form-list__items">
                    { for props.urls.iter().enumerate().map(|(index, url)| {
                        let on_edit = on_edit.clone();
                        html! {
                            <div class="tp__form-list__item tp__url-alias-list-item" key={format!("url-alias-{index}")}>
                                if props.readonly {
                                    <div class="tp__form-list__item-content">
                                        <span>{ url.to_string() }</span>
                                    </div>
                                } else {
                                    <div class="tp__form-list__item-toolbar">
                                        if index > 0 {
                                            <IconButton
                                                class="tp__form-list__item-arrow-btn"
                                                name={index.to_string()}
                                                icon="ArrowUp"
                                                hint={translate.t("LABEL.MOVE_UP")}
                                                aria_label={Some(translate.t("LABEL.MOVE_UP"))}
                                                onclick={on_move_up.clone()}
                                            />
                                        } else if props.urls.len() > 1 {
                                            <span class="tp__form-list__item-placeholder-btn"/>
                                        }
                                        if index + 1 < props.urls.len() {
                                            <IconButton
                                                class="tp__form-list__item-arrow-btn"
                                                name={index.to_string()}
                                                icon="ArrowDown"
                                                hint={translate.t("LABEL.MOVE_DOWN")}
                                                aria_label={Some(translate.t("LABEL.MOVE_DOWN"))}
                                                onclick={on_move_down.clone()}
                                            />
                                        } else if props.urls.len() > 1 {
                                            <span class="tp__form-list__item-placeholder-btn"/>
                                        }
                                    </div>
                                    <div class="tp__form-list__item-content">
                                        <Input
                                            name={format!("url_alias_{index}")}
                                            aria_label={Some(format!("{} {}", translate.t(LABEL_URL_ALIAS), index + 1))}
                                            value={url.to_string()}
                                            on_change={Callback::from(move |value: String| on_edit(index, value))}
                                            autocomplete={true}
                                        />
                                    </div>
                                    <IconButton
                                        class="tp__form-list__item-delete"
                                        name={index.to_string()}
                                        icon="Delete"
                                        hint={translate.t("LABEL.REMOVE")}
                                        aria_label={Some(translate.t("LABEL.REMOVE"))}
                                        onclick={on_remove.clone()}
                                    />
                                }
                            </div>
                        }
                    }) }
                </div>
                if !props.readonly {
                    <div class="tp__form-list__toolbar">
                        <TextButton
                            class="primary"
                            name="add_url_alias"
                            icon="Add"
                            title={translate.t(LABEL_ADD_URL_ALIAS)}
                            onclick={on_add}
                        />
                    </div>
                }
            </div>
        </Card>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn url_alias_editor_uses_the_native_design_system() {
        let source = include_str!("url_alias_form.rs");

        // Raw Unicode glyph buttons bypass AppIcon styling, focus states and theme colors.
        assert!(!source.contains(">{ \"↑\" }</button>"));
        assert!(!source.contains(">{ \"↓\" }</button>"));
        assert!(!source.contains(">{ \"×\" }</button>"));

        // All list actions use the same registered icon names as the rest of the editor.
        for icon in ["ArrowUp", "ArrowDown", "Delete", "Add"] {
            assert!(source.contains(&format!("icon=\"{icon}\"")), "missing native icon {icon}");
        }
        assert!(source.contains("<Input"), "URL values should use the native Input component");
    }

    /// The row's `aria-label` is built from `LABEL.URL_ALIAS`; a missing key reaches the user as
    /// `Key '...' not found for language 'en'` in the accessibility name.
    #[test]
    fn every_locale_defines_the_url_alias_label() {
        for (locale, json) in [
            ("en", include_str!("../../../../public/assets/i18n/en.json")),
            ("ru", include_str!("../../../../public/assets/i18n/ru.json")),
            ("ar", include_str!("../../../../public/assets/i18n/ar.json")),
            ("es", include_str!("../../../../public/assets/i18n/es.json")),
        ] {
            let value: serde_json::Value = serde_json::from_str(json).expect("valid locale json");
            let label = value.pointer("/LABEL/URL_ALIAS").and_then(serde_json::Value::as_str);
            assert!(label.is_some_and(|label| !label.trim().is_empty()), "{locale} is missing LABEL.URL_ALIAS");
        }
    }
}
