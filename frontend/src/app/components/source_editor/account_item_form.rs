use crate::{
    app::components::{
        build_options, format_date_input_value, input::Input, select::Select, selection_parse_first, Card,
        DropDownSelection, TextButton, ToolAction,
    },
    config_field, config_field_bool, config_field_child, config_field_custom, config_field_optional,
    config_field_optional_hide, edit_field_bool, edit_field_exp_date, edit_field_number_i16, edit_field_number_u16,
    edit_field_text, edit_field_text_option, generate_form_reducer,
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::{
    model::{
        ConfigInputAccountDto, ConfigProviderDto, InputType, ProxyConfigDto, StalkerAuthMode, StalkerDeviceProfileDto,
        StalkerEndpointPreference, StalkerInputConfigDto, StalkerMagPreset, XtreamLoginRequest,
    },
    utils::Internable,
};
use std::{rc::Rc, str::FromStr, sync::Arc};
use strum::IntoEnumIterator;
use yew::{platform::spawn_local, prelude::*};

const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_PASSWORD: &str = "LABEL.PASSWORD";
const LABEL_PRIORITY: &str = "LABEL.PRIORITY";
const LABEL_MAX_CONNECTIONS: &str = "LABEL.MAX_CONNECTIONS";
const LABEL_EXP_DATE: &str = "LABEL.EXP_DATE";
const LABEL_ENABLED: &str = "LABEL.ENABLED";

generate_form_reducer!(
    state: AccountFormState { form: ConfigInputAccountDto },
    action_name: AccountFormAction,
    fields {
        Enabled => enabled: bool,
        Username => username: Arc<str>,
        Password => password: Option<String>,
        Priority => priority: i16,
        MaxConnections => max_connections: u16,
        ExpDate => exp_date: Option<i64>,
        Stalker => stalker: Option<StalkerInputConfigDto>,
    }
);

fn stalker_account_choice<T, F>(
    form_state: &UseReducerHandle<AccountFormState>,
    name: &'static str,
    label: String,
    selected: T,
    update: F,
) -> Html
where
    T: IntoEnumIterator + ToString + FromStr + PartialEq + Clone + 'static,
    F: Fn(&mut StalkerInputConfigDto, T) + 'static,
{
    let state = form_state.clone();
    config_field_child!(label, "INPUT_FORM.STALKER_ALIAS_OVERRIDE", {
        let options = Rc::new(build_options(T::iter(), &selected, |value| html! { value.to_string() }));
        html! {
            <Select
                name={name}
                multi_select={false}
                on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                    if let Some(value) = selection_parse_first::<T>(&selections) {
                        let mut config = state.form.stalker.clone().unwrap_or_default();
                        update(&mut config, value);
                        state.dispatch(AccountFormAction::Stalker(Some(config)));
                    }
                })}
                options={options}
            />
        }
    })
}

#[derive(Properties, PartialEq, Clone)]
pub struct AccountItemFormProps {
    pub on_submit: Callback<ConfigInputAccountDto>,
    pub on_cancel: Callback<()>,
    pub input_type: InputType,
    #[prop_or_default]
    pub stalker_auth_mode: StalkerAuthMode,
    #[prop_or_default]
    pub providers: Vec<ConfigProviderDto>,
    #[prop_or_default]
    pub proxy: Option<ProxyConfigDto>,
    /// The address the owning input uses for this account (its primary lateral). The account
    /// itself has none, but resolving the expiration date still needs to reach the provider.
    #[prop_or_default]
    pub account_url: String,
    #[prop_or_default]
    pub initial: Option<ConfigInputAccountDto>,
    #[prop_or(false)]
    pub readonly: bool,
}

#[component]
pub fn AccountItemForm(props: &AccountItemFormProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let form_state: UseReducerHandle<AccountFormState> = use_reducer(|| AccountFormState {
        form: props.initial.clone().unwrap_or_else(|| ConfigInputAccountDto {
            id: 0,
            username: "".intern(),
            password: None,
            priority: 0,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
            url_aliases: None,
        }),
        modified: false,
    });
    let exp_date_loading = use_state(|| false);
    let exp_date_request_in_flight = use_mut_ref(|| false);
    let exp_date_request_token = use_mut_ref(|| 0_u64);
    let stalker_config = form_state.form.stalker.clone().unwrap_or_default();
    let stalker_device = stalker_config.device.clone().unwrap_or_default();
    let is_stalker = props.input_type.is_stalker();
    let effective_stalker_auth_mode =
        form_state.form.stalker.as_ref().map_or(props.stalker_auth_mode, |config| config.auth_mode);
    let show_credentials = !is_stalker
        || matches!(
            effective_stalker_auth_mode,
            StalkerAuthMode::CredentialsOnly | StalkerAuthMode::MacPlusCredentials
        );

    let handle_submit = {
        let form_state = form_state.clone();
        let on_submit = props.on_submit.clone();
        Callback::from(move |_| {
            let mut data = form_state.form.clone();
            if is_stalker && data.stalker.is_none() {
                data.stalker = Some(StalkerInputConfigDto::default());
            }
            if !data.username.trim().is_empty() {
                on_submit.emit(data);
            }
        })
    };

    let handle_cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_| {
            on_cancel.emit(());
        })
    };

    let exp_date_tool_action = if props.input_type.is_xtream() {
        let services = services.clone();
        let form_state = form_state.clone();
        let exp_date_loading = exp_date_loading.clone();
        let exp_date_request_in_flight = exp_date_request_in_flight.clone();
        let exp_date_request_token = exp_date_request_token.clone();
        let providers = props.providers.clone();
        let proxy = props.proxy.clone();
        let translate = translate.clone();
        let resolved_account_url = props.account_url.clone();

        Some(ToolAction {
            name: Some("RefreshAliasExpDate".to_string()),
            icon: "Refresh".to_string(),
            hint: Some(translate.t("LABEL.RESOLVE")),
            class: (*exp_date_loading).then(|| "loading".to_string()),
            onclick: Callback::from(move |_event: MouseEvent| {
                if *exp_date_request_in_flight.borrow() {
                    return;
                }

                let url = resolved_account_url.clone();
                let username = form_state.form.username.to_string();
                let password = form_state.form.password.clone().unwrap_or_default();

                if url.trim().is_empty() || username.trim().is_empty() || password.trim().is_empty() {
                    services.toastr.error(translate.t("MESSAGES.SOURCE_EDITOR.URL_USERNAME_AND_PASSWORD_MANDATORY"));
                    return;
                }

                *exp_date_request_in_flight.borrow_mut() = true;
                let request_token = {
                    let mut token = exp_date_request_token.borrow_mut();
                    *token += 1;
                    *token
                };
                exp_date_loading.set(true);
                let services = services.clone();
                let form_state = form_state.clone();
                let exp_date_loading = exp_date_loading.clone();
                let exp_date_request_in_flight = exp_date_request_in_flight.clone();
                let exp_date_request_token = exp_date_request_token.clone();
                let request = XtreamLoginRequest {
                    url,
                    username,
                    password,
                    proxy: proxy.clone(),
                    providers: (!providers.is_empty()).then_some(providers.clone()),
                };
                let no_exp_date_msg = translate.t("MESSAGES.SOURCE_EDITOR.NO_EXPIRATION_DATE_RETURNED");
                let account_url_for_snapshot = resolved_account_url.clone();

                spawn_local(async move {
                    let current_snapshot = || {
                        (
                            account_url_for_snapshot.clone(),
                            form_state.form.username.to_string(),
                            form_state.form.password.clone().unwrap_or_default(),
                        )
                    };
                    match services.config.get_xtream_login_info(&request).await {
                        Ok(login_info) => {
                            if *exp_date_request_token.borrow() == request_token {
                                let snapshot_matches = current_snapshot()
                                    == (request.url.clone(), request.username.clone(), request.password.clone());
                                if snapshot_matches {
                                    if let Some(exp_date) = login_info.exp_date {
                                        form_state.dispatch(AccountFormAction::ExpDate(Some(exp_date)));
                                    } else {
                                        services.toastr.warning(no_exp_date_msg);
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            if *exp_date_request_token.borrow() == request_token {
                                services.toastr.error(err.to_string());
                            }
                        }
                    }
                    if *exp_date_request_token.borrow() == request_token {
                        *exp_date_request_in_flight.borrow_mut() = false;
                        exp_date_loading.set(false);
                    }
                });
            }),
        })
    } else {
        None
    };

    html! {
        <Card class="tp__config-view__card tp__item-form">
            if props.readonly {
                { config_field_bool!(form_state.form, translate.t(LABEL_ENABLED), enabled) }
                if show_credentials {
                    <div class="tp__config-view__cols-2">
                      { config_field_custom!(translate.t(LABEL_USERNAME), form_state.form.username.to_string()) }
                      { config_field_optional_hide!(form_state.form, translate.t(LABEL_PASSWORD), password) }
                    </div>
                }
                <div class="tp__config-view__cols-2">
                  { config_field_custom!(translate.t(LABEL_PRIORITY), form_state.form.priority.to_string()) }
                  { config_field_custom!(translate.t(LABEL_MAX_CONNECTIONS), form_state.form.max_connections.to_string()) }
                </div>
            } else {
                <>
                    { edit_field_bool!(form_state, translate.t(LABEL_ENABLED), enabled, AccountFormAction::Enabled) }
                    if show_credentials {
                        <div class="tp__config-view__cols-2">
                          { edit_field_text!(form_state, translate.t(LABEL_USERNAME), username, AccountFormAction::Username) }
                          { edit_field_text_option!(form_state, translate.t(LABEL_PASSWORD), password, AccountFormAction::Password, true) }
                        </div>
                    }
                    <div class="tp__config-view__cols-2">
                      { edit_field_number_i16!(form_state, translate.t(LABEL_PRIORITY), priority, AccountFormAction::Priority) }
                      { edit_field_number_u16!(form_state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, AccountFormAction::MaxConnections) }
                    </div>
                </>
            }

            if props.input_type.is_stalker() {
                <div class="tp__config-view__cols-2">
                    if props.readonly {
                        { config_field_custom!(translate.t("LABEL.STALKER_MAC_ADDRESS"), stalker_device.mac_address.clone().unwrap_or_default()) }
                        { config_field_custom!(translate.t("LABEL.STALKER_AUTH_MODE"), effective_stalker_auth_mode.to_string()) }
                        { config_field_custom!(translate.t("LABEL.STALKER_MAG_PRESET"), stalker_config.mag_preset.to_string()) }
                        { config_field_custom!(translate.t("LABEL.STALKER_ENDPOINT_PREFERENCE"), stalker_config.endpoint_preference.to_string()) }
                    } else {
                        <div class="tp__form-field tp__form-field__text">
                            <Input label={translate.t("LABEL.STALKER_MAC_ADDRESS")} name="account_stalker_mac"
                                value={stalker_device.mac_address.clone().unwrap_or_default()}
                                on_change={{
                                    let form_state = form_state.clone();
                                    Callback::from(move |value: String| {
                                        let mut config = form_state.form.stalker.clone().unwrap_or_default();
                                        let device = config.device.get_or_insert_with(StalkerDeviceProfileDto::default);
                                        let value = value.trim().to_string();
                                        device.mac_address = (!value.is_empty()).then_some(value);
                                        form_state.dispatch(AccountFormAction::Stalker(Some(config)));
                                    })
                                }} />
                        </div>
                        { stalker_account_choice::<StalkerAuthMode, _>(
                            &form_state,
                            "account_stalker_auth_mode",
                            translate.t("LABEL.STALKER_AUTH_MODE"),
                            effective_stalker_auth_mode,
                            |config, value| config.auth_mode = value,
                        ) }
                        { stalker_account_choice::<StalkerMagPreset, _>(
                            &form_state,
                            "account_stalker_mag_preset",
                            translate.t("LABEL.STALKER_MAG_PRESET"),
                            stalker_config.mag_preset,
                            |config, value| config.mag_preset = value,
                        ) }
                        { stalker_account_choice::<StalkerEndpointPreference, _>(
                            &form_state,
                            "account_stalker_endpoint_preference",
                            translate.t("LABEL.STALKER_ENDPOINT_PREFERENCE"),
                            stalker_config.endpoint_preference,
                            |config, value| config.endpoint_preference = value,
                        ) }
                    }
                </div>
            }

            if props.readonly {
                { config_field_custom!(
                    translate.t(LABEL_EXP_DATE),
                    format_date_input_value(form_state.form.exp_date)
                ) }
            } else {
                { edit_field_exp_date!(form_state, translate.t(LABEL_EXP_DATE), exp_date, AccountFormAction::ExpDate, exp_date_tool_action) }
            }

            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_account"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                if !props.readonly {
                    <TextButton
                        class="primary"
                        name="submit_account"
                        icon="Accept"
                        title={translate.t("LABEL.SUBMIT")}
                        onclick={handle_submit}
                    />
                }
            </div>
        </Card>
    }
}
