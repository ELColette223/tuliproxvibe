use crate::{
    services::{get_base_href, get_token},
    utils::set_timeout,
};
use log::{error, trace, warn};
use shared::{
    model::{LogEntry, LogLevel, LogWsMessage},
    utils::concat_path_leading_slash,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};
use yew::prelude::*;

const WS_RECONNECT_BASE_MS: i32 = 300;
const WS_RECONNECT_MAX_MS: i32 = 3000;
const DEFAULT_MAX_LOG_LINES: usize = 2000;
/// Coalesce high-frequency TRACE/DEBUG bursts into one React/Yew update.
const LOG_FLUSH_INTERVAL_MS: i32 = 100;

fn reconnect_delay(attempt: u16) -> i32 {
    if attempt < 6 {
        let d = WS_RECONNECT_BASE_MS * (i32::from(attempt) + 1);
        d.min(WS_RECONNECT_MAX_MS)
    } else {
        WS_RECONNECT_MAX_MS
    }
}

fn append_capped(list: &mut Vec<LogEntry>, incoming: impl IntoIterator<Item = LogEntry>, max_lines: usize) {
    list.extend(incoming);
    if list.len() > max_lines {
        let skip = list.len() - max_lines;
        list.drain(0..skip);
    }
}

pub struct UseLogStreamOptions {
    pub active: bool,
    pub max_lines: usize,
}

impl Default for UseLogStreamOptions {
    fn default() -> Self { Self { active: true, max_lines: DEFAULT_MAX_LOG_LINES } }
}

#[derive(Clone)]
pub struct UseLogStreamHandle {
    pub connected: bool,
    pub logs: Rc<Vec<LogEntry>>,
    pub min_level: Option<LogLevel>,
    pub set_min_level: Callback<Option<LogLevel>>,
    pub clear: Callback<()>,
}

#[hook]
pub fn use_log_stream(options: UseLogStreamOptions) -> UseLogStreamHandle {
    let connected = use_state(|| false);
    let logs = use_state(|| Rc::new(Vec::<LogEntry>::new()));
    // Default Info so TRACE/DEBUG server config cannot flood the UI unless the user opts into ALL/TRACE.
    let min_level = use_state(|| Some(LogLevel::Info));
    let min_level_current = use_mut_ref(|| Some(LogLevel::Info));

    let ws_cell = use_mut_ref(|| None::<WebSocket>);
    let attempt_cell = use_mut_ref(|| 0u16);
    let active_cell = use_mut_ref(|| options.active);
    let pending_entries = use_mut_ref(VecDeque::<LogEntry>::new);
    let flush_scheduled = use_mut_ref(|| false);
    let max_lines = options.max_lines;

    *active_cell.borrow_mut() = options.active;

    let clear = {
        let logs = logs.clone();
        let pending_entries = pending_entries.clone();
        Callback::from(move |()| {
            pending_entries.borrow_mut().clear();
            logs.set(Rc::new(Vec::new()));
        })
    };

    let set_min_level = {
        let min_level_state = min_level.clone();
        let min_level_current = min_level_current.clone();
        let ws_cell = ws_cell.clone();
        Callback::from(move |lvl: Option<LogLevel>| {
            *min_level_current.borrow_mut() = lvl;
            min_level_state.set(lvl);
            if let Some(ws) = ws_cell.borrow().as_ref() {
                if ws.ready_state() == WebSocket::OPEN {
                    let msg = LogWsMessage::Filter { min_level: lvl };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        let _ = ws.send_with_str(&json);
                    }
                }
            }
        })
    };

    {
        let connected = connected.clone();
        let logs = logs.clone();
        let min_level_current = min_level_current.clone();
        let ws_cell = ws_cell.clone();
        let attempt_cell = attempt_cell.clone();
        let pending_entries = pending_entries.clone();
        let flush_scheduled = flush_scheduled.clone();
        let active = options.active;

        use_effect_with(active, move |&active| {
            if !active {
                pending_entries.borrow_mut().clear();
                *flush_scheduled.borrow_mut() = false;
                if let Some(ws) = ws_cell.borrow_mut().take() {
                    ws.set_onmessage(None);
                    ws.set_onopen(None);
                    ws.set_onerror(None);
                    ws.set_onclose(None);
                    let _ = ws.close();
                }
                connected.set(false);
                return Box::new(|| {}) as Box<dyn FnOnce()>;
            }

            let is_alive = Rc::new(Cell::new(true));

            type ConnectFn = Rc<dyn Fn()>;
            type ConnectFnCell = Rc<RefCell<Option<ConnectFn>>>;

            let connect_fn: ConnectFnCell = Rc::new(RefCell::new(None));
            let connect_fn_clone = connect_fn.clone();

            let do_connect = {
                let connected = connected.clone();
                let logs = logs.clone();
                let min_level_current = min_level_current.clone();
                let ws_cell = ws_cell.clone();
                let attempt_cell = attempt_cell.clone();
                let pending_entries = pending_entries.clone();
                let flush_scheduled = flush_scheduled.clone();
                let is_alive = is_alive.clone();
                let connect_fn = connect_fn_clone.clone();

                Rc::new(move || {
                    if !is_alive.get() {
                        return;
                    }

                    if let Some(ws) = ws_cell.borrow().as_ref() {
                        let state = ws.ready_state();
                        if state == WebSocket::CONNECTING || state == WebSocket::OPEN {
                            return;
                        }
                    }

                    let base_href = get_base_href();
                    let path = concat_path_leading_slash(&base_href, "ws/logs");
                    let token_opt = get_token();
                    let mut ws_url = if let Some(ref token) = token_opt {
                        format!("{path}?token={token}")
                    } else {
                        path
                    };
                    if let Some(lvl) = *min_level_current.borrow() {
                        let sep = if ws_url.contains('?') { '&' } else { '?' };
                        ws_url.push_str(&format!("{sep}min_level={}", lvl.as_str()));
                    }

                    match WebSocket::new(&ws_url) {
                        Err(err) => {
                            error!("Failed to create log websocket: {err:?}");
                            let attempt = *attempt_cell.borrow();
                            *attempt_cell.borrow_mut() = attempt.saturating_add(1);
                            let delay = reconnect_delay(attempt);
                            let connect_fn_inner = connect_fn.clone();
                            set_timeout(
                                move || {
                                    if let Some(f) = connect_fn_inner.borrow().as_ref() {
                                        f();
                                    }
                                },
                                delay,
                            );
                        }
                        Ok(socket) => {
                            *ws_cell.borrow_mut() = Some(socket.clone());

                            {
                                let socket_clone = socket.clone();
                                let token_opt = token_opt.clone();
                                let min_level_current = min_level_current.clone();
                                let attempt_cell = attempt_cell.clone();
                                let is_alive = is_alive.clone();

                                let onopen = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_| {
                                    if !is_alive.get() {
                                        return;
                                    }
                                    trace!("Log WebSocket connected");
                                    *attempt_cell.borrow_mut() = 0;

                                    if let Some(ref token) = token_opt {
                                        let auth_msg = LogWsMessage::Auth(token.clone());
                                        if let Ok(json) = serde_json::to_string(&auth_msg) {
                                            let _ = socket_clone.send_with_str(&json);
                                        }
                                    }

                                    // Query may already carry min_level; also send Filter for runtime changes / auth path.
                                    let filter_msg = LogWsMessage::Filter { min_level: *min_level_current.borrow() };
                                    if let Ok(json) = serde_json::to_string(&filter_msg) {
                                        let _ = socket_clone.send_with_str(&json);
                                    }
                                }));
                                socket.set_onopen(Some(onopen.as_ref().unchecked_ref()));
                                onopen.forget();
                            }

                            {
                                let connected = connected.clone();
                                let logs = logs.clone();
                                let pending_entries = pending_entries.clone();
                                let flush_scheduled = flush_scheduled.clone();
                                let is_alive = is_alive.clone();

                                let onmessage =
                                    Closure::<dyn FnMut(MessageEvent)>::wrap(Box::new(move |e: MessageEvent| {
                                        if !is_alive.get() {
                                            return;
                                        }
                                        if let Ok(text) = e.data().dyn_into::<js_sys::JsString>() {
                                            let text_str: String = text.into();
                                            if let Ok(msg) = serde_json::from_str::<LogWsMessage>(&text_str) {
                                                match msg {
                                                    LogWsMessage::Authorized => {
                                                        connected.set(true);
                                                    }
                                                    LogWsMessage::Unauthorized => {
                                                        connected.set(false);
                                                        warn!("Log WebSocket unauthorized");
                                                    }
                                                    LogWsMessage::History(entries) => {
                                                        connected.set(true);
                                                        pending_entries.borrow_mut().clear();
                                                        *flush_scheduled.borrow_mut() = false;
                                                        let mut new_logs = entries;
                                                        if new_logs.len() > max_lines {
                                                            let skip = new_logs.len() - max_lines;
                                                            new_logs.drain(0..skip);
                                                        }
                                                        logs.set(Rc::new(new_logs));
                                                    }
                                                    LogWsMessage::Entry(entry) => {
                                                        connected.set(true);
                                                        pending_entries.borrow_mut().push_back(entry);
                                                        if *flush_scheduled.borrow() {
                                                            return;
                                                        }
                                                        *flush_scheduled.borrow_mut() = true;

                                                        let logs = logs.clone();
                                                        let pending_entries = pending_entries.clone();
                                                        let flush_scheduled = flush_scheduled.clone();
                                                        let is_alive = is_alive.clone();
                                                        set_timeout(
                                                            move || {
                                                                *flush_scheduled.borrow_mut() = false;
                                                                if !is_alive.get() {
                                                                    pending_entries.borrow_mut().clear();
                                                                    return;
                                                                }
                                                                let mut pending = pending_entries.borrow_mut();
                                                                if pending.is_empty() {
                                                                    return;
                                                                }
                                                                let batch: Vec<LogEntry> = pending.drain(..).collect();
                                                                drop(pending);
                                                                logs.set({
                                                                    let mut list = (**logs).clone();
                                                                    append_capped(&mut list, batch, max_lines);
                                                                    Rc::new(list)
                                                                });
                                                            },
                                                            LOG_FLUSH_INTERVAL_MS,
                                                        );
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }));
                                socket.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
                                onmessage.forget();
                            }

                            {
                                let connected = connected.clone();
                                let ws_cell = ws_cell.clone();
                                let attempt_cell = attempt_cell.clone();
                                let is_alive = is_alive.clone();
                                let connect_fn_inner = connect_fn.clone();

                                let onclose = Closure::<dyn FnMut(CloseEvent)>::wrap(Box::new(move |_| {
                                    if !is_alive.get() {
                                        return;
                                    }
                                    trace!("Log WebSocket closed");
                                    connected.set(false);
                                    *ws_cell.borrow_mut() = None;

                                    let attempt = *attempt_cell.borrow();
                                    *attempt_cell.borrow_mut() = attempt.saturating_add(1);
                                    let delay = reconnect_delay(attempt);
                                    let connect_fn_reconnect = connect_fn_inner.clone();
                                    set_timeout(
                                        move || {
                                            if let Some(f) = connect_fn_reconnect.borrow().as_ref() {
                                                f();
                                            }
                                        },
                                        delay,
                                    );
                                }));
                                socket.set_onclose(Some(onclose.as_ref().unchecked_ref()));
                                onclose.forget();
                            }

                            {
                                let onerror = Closure::<dyn FnMut(ErrorEvent)>::wrap(Box::new(move |e: ErrorEvent| {
                                    trace!("Log WebSocket error: {e:?}");
                                }));
                                socket.set_onerror(Some(onerror.as_ref().unchecked_ref()));
                                onerror.forget();
                            }
                        }
                    }
                })
            };

            *connect_fn.borrow_mut() = Some(do_connect.clone());
            do_connect();

            Box::new(move || {
                is_alive.set(false);
                pending_entries.borrow_mut().clear();
                *flush_scheduled.borrow_mut() = false;
                if let Some(ws) = ws_cell.borrow_mut().take() {
                    ws.set_onmessage(None);
                    ws.set_onopen(None);
                    ws.set_onerror(None);
                    ws.set_onclose(None);
                    let _ = ws.close();
                }
            }) as Box<dyn FnOnce()>
        });
    }

    UseLogStreamHandle { connected: *connected, logs: (*logs).clone(), min_level: *min_level, set_min_level, clear }
}

#[cfg(test)]
mod tests {
    use super::append_capped;
    use shared::model::{LogEntry, LogLevel};

    #[test]
    fn append_capped_trims_from_front() {
        let mut list = Vec::new();
        for i in 0..5 {
            list.push(LogEntry::new("ts", LogLevel::Info, "t", format!("old-{i}")));
        }
        append_capped(
            &mut list,
            (0..3).map(|i| LogEntry::new("ts", LogLevel::Info, "t", format!("new-{i}"))),
            5,
        );
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].message, "old-3");
        assert_eq!(list[4].message, "new-2");
    }
}
