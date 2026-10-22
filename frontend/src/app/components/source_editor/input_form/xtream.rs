use super::{common::CommonInputForm, ConfigInputFormAction, ConfigInputFormState, LABEL_RESOLVE};
use crate::{
    app::components::{input::Input, Card, TitledCard, ToolAction},
    config_field, config_field_optional, config_field_optional_hide,
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::model::{ConfigProviderDto, XtreamLoginRequest};
use web_sys::MouseEvent;
use yew::{
    component, html, platform::spawn_local, use_mut_ref, use_state, Callback, Html, Properties, UseReducerHandle,
};

#[derive(Properties, Clone)]
pub(super) struct XtreamInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub providers: Vec<ConfigProviderDto>,
    pub allow_write: bool,
}

impl PartialEq for XtreamInputFormProps {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

#[component]
pub(super) fn XtreamInputForm(props: &XtreamInputFormProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let loading = use_state(|| false);
    let request_in_flight = use_mut_ref(|| false);
    let request_token = use_mut_ref(|| 0_u64);
    let state = props.state.clone();

    let exp_date_tool_action = {
        let services = services.clone();
        let translate = translate.clone();
        let state = state.clone();
        let providers = props.providers.clone();
        let loading = loading.clone();
        let request_in_flight = request_in_flight.clone();
        let request_token = request_token.clone();
        ToolAction {
            name: Some("RefreshExpDate".to_string()),
            icon: "Refresh".to_string(),
            hint: Some(translate.t(LABEL_RESOLVE)),
            class: (*loading).then(|| "loading".to_string()),
            onclick: Callback::from(move |_event: MouseEvent| {
                if *request_in_flight.borrow() {
                    return;
                }
                let url = state.form.url.clone();
                let username = state.form.username.clone().unwrap_or_default();
                let password = state.form.password.clone().unwrap_or_default();
                if url.trim().is_empty() || username.trim().is_empty() || password.trim().is_empty() {
                    services.toastr.error(translate.t("MESSAGES.SOURCE_EDITOR.URL_USERNAME_AND_PASSWORD_MANDATORY"));
                    return;
                }
                *request_in_flight.borrow_mut() = true;
                let token = {
                    let mut current = request_token.borrow_mut();
                    *current += 1;
                    *current
                };
                loading.set(true);
                let request = XtreamLoginRequest {
                    url,
                    username,
                    password,
                    proxy: state.form.proxy.clone(),
                    providers: (!providers.is_empty()).then_some(providers.clone()),
                };
                let services = services.clone();
                let state = state.clone();
                let loading = loading.clone();
                let request_in_flight = request_in_flight.clone();
                let request_token = request_token.clone();
                let translate = translate.clone();
                spawn_local(async move {
                    match services.config.get_xtream_login_info(&request).await {
                        Ok(login_info) if *request_token.borrow() == token => {
                            let unchanged = state.form.url == request.url
                                && state.form.username.as_deref().unwrap_or_default() == request.username
                                && state.form.password.as_deref().unwrap_or_default() == request.password;
                            if unchanged {
                                if let Some(exp_date) = login_info.exp_date {
                                    state.dispatch(ConfigInputFormAction::ExpDate(Some(exp_date)));
                                } else {
                                    services
                                        .toastr
                                        .warning(translate.t("MESSAGES.SOURCE_EDITOR.NO_EXPIRATION_DATE_RETURNED"));
                                }
                            }
                        }
                        Err(err) if *request_token.borrow() == token => services.toastr.error(err.to_string()),
                        Ok(_) | Err(_) => {}
                    }
                    if *request_token.borrow() == token {
                        *request_in_flight.borrow_mut() = false;
                        loading.set(false);
                    }
                });
            }),
        }
    };

    let proxy = state.form.proxy.clone().unwrap_or_default();
    let has_proxy = !proxy.is_empty();
    let proxy_form = if !has_proxy && !props.allow_write {
        html! {}
    } else if props.allow_write {
        let on_proxy_url = {
            let state = state.clone();
            Callback::from(move |value: String| {
                let mut proxy = state.form.proxy.clone().unwrap_or_default();
                proxy.url = value;
                state.dispatch(ConfigInputFormAction::Proxy((!proxy.is_empty()).then_some(proxy)));
            })
        };
        let on_proxy_username = {
            let state = state.clone();
            Callback::from(move |value: String| {
                let mut proxy = state.form.proxy.clone().unwrap_or_default();
                proxy.username = (!value.is_empty()).then_some(value);
                state.dispatch(ConfigInputFormAction::Proxy((!proxy.is_empty()).then_some(proxy)));
            })
        };
        let on_proxy_password = {
            let state = state.clone();
            Callback::from(move |value: String| {
                let mut proxy = state.form.proxy.clone().unwrap_or_default();
                proxy.password = (!value.is_empty()).then_some(value);
                state.dispatch(ConfigInputFormAction::Proxy((!proxy.is_empty()).then_some(proxy)));
            })
        };
        html! {
            <Card class="tp__config-view__card">
                <TitledCard title={translate.t("LABEL.PROXY")}>
                    <Input label={translate.t("LABEL.URL")} name="proxy_url" value={proxy.url}
                        hint_key={Some("INPUT_FORM.PROXY".to_string())} on_change={on_proxy_url} />
                    <div class="tp__config-view__cols-2">
                        <Input label={translate.t("LABEL.USERNAME")} name="proxy_username"
                            value={proxy.username.unwrap_or_default()} on_change={on_proxy_username} />
                        <Input label={translate.t("LABEL.PASSWORD")} name="proxy_password" hidden={true}
                            value={proxy.password.unwrap_or_default()} on_change={on_proxy_password} />
                    </div>
                </TitledCard>
            </Card>
        }
    } else {
        html! {
            <Card class="tp__config-view__card">
                <TitledCard title={translate.t("LABEL.PROXY")}>
                    { config_field!(proxy, translate.t("LABEL.URL"), url, Some("INPUT_FORM.PROXY".to_string())) }
                    <div class="tp__config-view__cols-2">
                        { config_field_optional!(proxy, translate.t("LABEL.USERNAME"), username) }
                        { config_field_optional_hide!(proxy, translate.t("LABEL.PASSWORD"), password) }
                    </div>
                </TitledCard>
            </Card>
        }
    };

    html! { <>
        <CommonInputForm state={state} allow_write={props.allow_write} credentials={true}
            connection={true} cache_duration={true} exp_date_tool_action={Some(exp_date_tool_action)} />
        {proxy_form}
    </> }
}
