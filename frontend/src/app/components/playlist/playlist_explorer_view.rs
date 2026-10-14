use crate::{
    app::{
        components::{playlist::playlist_explorer::PlaylistExplorer, PlaylistSourceSelector},
        context::PlaylistExplorerContext,
    },
    hooks::use_service_context,
    model::EventMessage,
};
use shared::model::{PlaylistRequest, PlaylistUpdateState, UiPlaylistCategories};
use std::rc::Rc;
use yew::{platform::spawn_local, prelude::*};

fn should_refresh_explorer(request: Option<&PlaylistRequest>, state: PlaylistUpdateState) -> bool {
    matches!(request, Some(PlaylistRequest::Target(_) | PlaylistRequest::Input(_)))
        && state != PlaylistUpdateState::Failure
}

#[component]
pub fn PlaylistExplorerView() -> Html {
    let playlist = use_state(|| None::<Rc<UiPlaylistCategories>>);
    let playlist_req = use_state(|| None::<PlaylistRequest>);
    let services = use_service_context();
    // Incremented for each request so an older refresh cannot overwrite a newer selection.
    let request_revision = use_mut_ref(|| 0_u64);

    {
        let playlist = playlist.clone();
        let playlist_req = playlist_req.clone();
        let services = services.clone();
        let request_revision = request_revision.clone();
        use_effect_with((*playlist_req).clone(), move |selected_request| {
            // A new selection invalidates any refresh still in flight.
            *request_revision.borrow_mut() += 1;
            let selected_request = selected_request.clone();
            let services_for_cleanup = services.clone();
            let services_for_events = services.clone();
            let revision_for_events = request_revision.clone();
            let sub_id = services.event.subscribe(move |message| {
                let EventMessage::PlaylistUpdate(update) = message else { return };
                if !should_refresh_explorer(selected_request.as_ref(), update.state) { return; }
                let Some(request) = selected_request.clone() else { return };
                let playlist = playlist.clone();
                let services = services_for_events.clone();
                let request_revision = revision_for_events.clone();
                let revision = {
                    let mut current = request_revision.borrow_mut();
                    *current += 1;
                    *current
                };
                spawn_local(async move {
                    let refreshed = services.playlist.get_playlist_categories(&request).await;
                    if *request_revision.borrow() == revision {
                        if let Some(refreshed) = refreshed { playlist.set(Some(refreshed)); }
                    }
                });
            });
            move || services_for_cleanup.event.unsubscribe(sub_id)
        });
    }

    let context = PlaylistExplorerContext { playlist: playlist.clone(), playlist_request: playlist_req.clone() };

    html! {
        <ContextProvider<PlaylistExplorerContext> context={context}>
          <div class="tp__playlist-explorer-view tp__list-view">
            <div class="tp__playlist-explorer-view__body tp__list-view__body">
                <div class={"tp__panel"}>
                    <PlaylistSourceSelector />
                    <PlaylistExplorer />
                </div>
            </div>
        </div>
        </ContextProvider<PlaylistExplorerContext>>
    }
}

#[cfg(test)]
mod tests {
    use super::should_refresh_explorer;
    use shared::model::{PlaylistRequest, PlaylistUpdateState};

    #[test]
    fn refresh_only_selected_local_playlists_after_nonfailed_updates() {
        for request in [PlaylistRequest::Target(5), PlaylistRequest::Input("provider".into())] {
            assert!(should_refresh_explorer(Some(&request), PlaylistUpdateState::Success));
            assert!(should_refresh_explorer(Some(&request), PlaylistUpdateState::Partial));
            assert!(!should_refresh_explorer(Some(&request), PlaylistUpdateState::Failure));
        }
        assert!(!should_refresh_explorer(None, PlaylistUpdateState::Success));
        assert!(!should_refresh_explorer(
            Some(&PlaylistRequest::CustomM3u(shared::model::PlaylistRequestM3u { url: "https://example.com".into() })),
            PlaylistUpdateState::Success,
        ));
    }
}
