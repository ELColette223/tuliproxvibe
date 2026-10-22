use crate::{
    app::components::{Card, TextButton},
    i18n::use_translation,
};
use shared::utils::Internable;
use std::sync::Arc;
use yew::prelude::*;

const LABEL_URL_ALIASES: &str = "LABEL.URL_ALIASES";
const LABEL_ADD_URL_ALIAS: &str = "LABEL.ADD_URL_ALIAS";
const LABEL_REMOVE: &str = "LABEL.REMOVE";

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
        move |index: usize| {
            let mut next = urls.clone();
            if index < next.len() {
                next.remove(index);
            }
            emit(next, &on_change);
        }
    };

    let on_move = {
        let urls = props.urls.clone();
        let on_change = props.on_change.clone();
        move |index: usize, offset: isize| {
            let target = index as isize + offset;
            if target < 0 || target as usize >= urls.len() {
                return;
            }
            let mut next = urls.clone();
            next.swap(index, target as usize);
            emit(next, &on_change);
        }
    };

    let on_add = {
        let urls = props.urls.clone();
        let on_change = props.on_change.clone();
        move |_: ()| {
            let mut next = urls.clone();
            next.push("".intern());
            on_change.emit(next);
        }
    };

    html! {
        <Card class="tp__config-view__card tp__item-form">
            <div class="tp__form-field">
                <label class="tp__form-field__label">{ translate.t(LABEL_URL_ALIASES) }</label>
            </div>
            <div class="tp__form-list">
                <div class="tp__form-list__items">
                    { for props.urls.iter().enumerate().map(|(index, url)| {
                        let on_move_up = on_move.clone();
                        let on_move_down = on_move.clone();
                        let on_remove = on_remove.clone();
                        let on_edit = on_edit.clone();
                        html! {
                            <div class="tp__form-list__item" key={format!("url-alias-{index}")}>
                                <div class="tp__form-list__item-toolbar">
                                    <button
                                        type="button"
                                        class="tp__form-list__arrow-btn"
                                        disabled={props.readonly || index == 0}
                                        title={translate.t("LABEL.MOVE_UP")}
                                        onclick={Callback::from(move |_| on_move_up(index, -1))}
                                    >{ "↑" }</button>
                                    <button
                                        type="button"
                                        class="tp__form-list__arrow-btn"
                                        disabled={props.readonly || index + 1 >= props.urls.len()}
                                        title={translate.t("LABEL.MOVE_DOWN")}
                                        onclick={Callback::from(move |_| on_move_down(index, 1))}
                                    >{ "↓" }</button>
                                </div>
                                <div class="tp__form-list__item-content">
                                    if props.readonly {
                                        <span>{ url.to_string() }</span>
                                    } else {
                                        <input
                                            class="tp__input"
                                            type="text"
                                            value={url.to_string()}
                                            oninput={Callback::from(move |event: InputEvent| {
                                                let input: web_sys::HtmlInputElement = event.target_unchecked_into();
                                                on_edit(index, input.value());
                                            })}
                                        />
                                    }
                                </div>
                                if !props.readonly {
                                    <button
                                        type="button"
                                        class="tp__form-list__arrow-btn"
                                        title={translate.t(LABEL_REMOVE)}
                                        onclick={Callback::from(move |_| on_remove(index))}
                                    >{ "×" }</button>
                                }
                            </div>
                        }
                    }) }
                </div>
                if !props.readonly {
                    <TextButton
                        class="secondary"
                        name="add_url_alias"
                        icon="Add"
                        title={translate.t(LABEL_ADD_URL_ALIAS)}
                        onclick={Callback::from(move |_: String| on_add(()))}
                    />
                }
            </div>
        </Card>
    }
}
