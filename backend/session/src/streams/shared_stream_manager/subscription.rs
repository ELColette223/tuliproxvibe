use super::{
    buffer::{send_burst_buffer, send_client_chunk, SendOutcome, SubscriberId},
    BudgetedChunk, BurstBuffer, PendingJoinGuard, PendingSharedSubscriberCleanup, ReceiverStreamWrapper,
    SharedStreamState, SharedSubscriber, TerminalOutcome, DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS,
    MAX_SUBSCRIBER_LIVE_BATCH_CHUNKS, SHARED_BURST_BYTES_PER_BUFFER_SLOT,
};
use crate::{
    streams::buffered_stream::CHANNEL_SIZE, BoxedProviderStream, CleanupEvent, ConnectionManager,
    SharedCleanupCapability, PROVIDER_END_NOT_SET,
};
use futures::StreamExt;
use log::{debug, warn};
use shared::utils::sanitize_sensitive_info;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    sync::{mpsc, oneshot, Mutex, Notify, RwLock, Semaphore},
    time::{sleep, Duration, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{ProviderHandle, StreamError},
    utils::{debug_if_enabled, trace_if_enabled},
};

impl SharedStreamState {
    pub(super) fn new(
        headers: Vec<(String, String)>,
        buf_size: usize,
        provider_guard: Option<ProviderHandle>,
        min_burst_buffer_size: usize,
        low_priority_preempted: Option<tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer>,
    ) -> Self {
        let base_channel_capacity = buf_size.max(CHANNEL_SIZE);
        let burst_buffer_size_in_bytes =
            min_burst_buffer_size.max(base_channel_capacity.saturating_mul(SHARED_BURST_BYTES_PER_BUFFER_SLOT));
        Self {
            headers,
            buf_size: base_channel_capacity,
            provider_guard,
            low_priority_preempted,
            preempted_token: CancellationToken::new(),
            subscribers: RwLock::new(HashMap::new()),
            stop_token: CancellationToken::new(),
            burst_buffer: Arc::new(Mutex::new(BurstBuffer::new(burst_buffer_size_in_bytes))),
            live_notification: Arc::new(Notify::new()),
            terminal_outcome: Arc::new(std::sync::Mutex::new(None)),
            task_handles: std::sync::Mutex::new(Vec::new()),
            subscriber_idle_timeout_secs: DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS,
            subscriber_max_duration: None,
            pending_joins: AtomicUsize::new(0),
        }
    }

    pub(super) fn with_subscriber_idle_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.subscriber_idle_timeout_secs = secs;
        }
        self
    }

    pub(super) fn increment_pending_joins(&self) { self.pending_joins.fetch_add(1, Ordering::AcqRel); }

    pub(super) fn decrement_pending_joins(&self) { self.pending_joins.fetch_sub(1, Ordering::AcqRel); }

    pub(super) fn has_pending_joins(&self) -> bool { self.pending_joins.load(Ordering::Acquire) > 0 }

    pub(super) fn lock_terminal_outcome(&self) -> std::sync::MutexGuard<'_, Option<TerminalOutcome>> {
        self.terminal_outcome.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn set_terminal_outcome(&self, outcome: TerminalOutcome) {
        let mut guard = self.lock_terminal_outcome();
        if guard.is_none() {
            *guard = Some(outcome);
        }
    }

    pub(super) fn terminal_is_failure(&self) -> bool {
        self.lock_terminal_outcome().as_ref().is_some_and(TerminalOutcome::is_failure)
    }

    pub(super) fn lock_task_handles(&self) -> std::sync::MutexGuard<'_, Vec<tokio::task::JoinHandle<()>>> {
        self.task_handles.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) async fn register_subscriber(
        &self,
        id: SubscriberId,
        addr: &SocketAddr,
        cancel_token: CancellationToken,
    ) {
        self.subscribers.write().await.insert(id, SharedSubscriber { addr: *addr, cancel_token });
    }

    pub(super) async fn cancel_subscribers(&self) {
        for subscriber in self.subscribers.read().await.values() {
            subscriber.cancel_token.cancel();
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn subscribe(
        self: &Arc<Self>,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        connection_manager: Arc<ConnectionManager>,
        mut pending_cleanup: PendingSharedSubscriberCleanup,
        pending_join: PendingJoinGuard,
    ) -> (BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability) {
        let (client_tx, client_rx) = mpsc::channel::<BudgetedChunk>(self.buf_size);
        let cancel_token = CancellationToken::new();
        let queue_byte_budget = self.buf_size.saturating_mul(SHARED_BURST_BYTES_PER_BUFFER_SLOT);
        let byte_budget = Arc::new(Semaphore::new(queue_byte_budget));

        {
            let mut handles = self.lock_task_handles();
            handles.retain(|h| !h.is_finished());
        }

        self.register_subscriber(subscriber_id, addr, cancel_token.clone()).await;
        pending_join.commit();
        let (start_tx, start_rx) = oneshot::channel();
        let cleanup_manager = Arc::clone(&connection_manager);

        let client_tx_clone = client_tx.clone();
        let burst_buffer = Arc::clone(&self.burst_buffer);
        let burst_buffer_for_log = Arc::clone(&self.burst_buffer);
        let live_notification = Arc::clone(&self.live_notification);
        // Subscriber-local terminal error (e.g. lag) shared with the response wrapper so
        // the client receives the error before the reserved cleanup is classified.
        let subscriber_terminal_error = Arc::new(std::sync::Mutex::new(None::<StreamError>));
        let forwarder_terminal_error_slot = Arc::clone(&subscriber_terminal_error);
        let forwarder_cancel_token = cancel_token.clone();
        let origin_state = Arc::clone(self);
        let timeout_duration = Duration::from_secs(self.subscriber_idle_timeout_secs);
        let idle_check_interval = Duration::from_secs(1);
        let subscriber_buf_size = self.buf_size;
        let preempted_token = self.preempted_token.clone();
        let low_priority_preempted = self.low_priority_preempted.clone();
        let address = *addr;
        let subscriber_started_at = Instant::now();

        let handle = tokio::spawn(async move {
            // Wait for the response to be polled, but respect cancellation/shutdown so a
            // body that is never polled cannot keep the forwarder task alive indefinitely.
            tokio::select! {
                () = cancel_token.cancelled() => return,
                started = start_rx => {
                    if started.is_err() {
                        return;
                    }
                }
            }
            // Send-progress budget begins only once the body is actually polled.
            let mut last_active = Instant::now();
            let (snapshot, mut next_sequence) = {
                let buffer = burst_buffer.lock().await;
                buffer.snapshot()
            };
            match send_burst_buffer(&snapshot, &client_tx_clone, &cancel_token, timeout_duration, &byte_budget).await {
                Ok(sent_burst_chunks) => {
                    drop(snapshot);
                    if sent_burst_chunks > 0 {
                        // The replay renewed its own internal progress time; carry that
                        // forward so the first live send does not immediately expire.
                        last_active = Instant::now();
                        debug_if_enabled!(
                            "Shared stream subscriber {} replayed {sent_burst_chunks} burst chunks after {} ms",
                            sanitize_sensitive_info(&address.to_string()),
                            subscriber_started_at.elapsed().as_millis()
                        );
                    }
                }
                Err(outcome) => {
                    drop(snapshot);
                    debug!(
                        "Shared stream subscriber {} burst replay failed ({outcome:?}); terminating",
                        sanitize_sensitive_info(&address.to_string())
                    );
                    cancel_token.cancel();
                    cleanup_manager.send_cleanup(CleanupEvent::ReleaseSharedSubscriber {
                        addr: address,
                        subscriber_id,
                        request_id: None,
                        owner: None,
                        provider_end_reason: PROVIDER_END_NOT_SET,
                        provider_error_class: None,
                    });
                    return;
                }
            }

            let mut first_live_chunk_logged = false;
            let mut startup_chunks_sent = 0_usize;
            let mut startup_bytes_sent = 0_usize;
            let mut startup_stats_logged = false;
            let mut read_chunks = Vec::with_capacity(subscriber_buf_size.min(64));
            let idle_check = sleep(idle_check_interval);
            tokio::pin!(idle_check);

            loop {
                // Pre-create the notified future before locking the buffer to avoid
                // a race where notify_waiters() fires between lock release and await.
                let notified_fut = live_notification.notified();

                let read = {
                    let buffer = burst_buffer.lock().await;
                    buffer.read_from_into(next_sequence, &mut read_chunks, MAX_SUBSCRIBER_LIVE_BATCH_CHUNKS)
                };
                next_sequence = read.next_sequence;
                if read.skipped > 0 {
                    // Subscriber fell behind the burst buffer - fail closed.
                    // Only this subscriber stops; the origin and the other subscribers continue.
                    // The lag error is recorded in the shared slot so the response wrapper can
                    // deliver `SubscriberLagged` to the client and only then classify its
                    // reserved cleanup. The cancel token must stay uncancelled for that.
                    let (ring_chunks, buffered_bytes) = {
                        let buffer = burst_buffer_for_log.lock().await;
                        (buffer.buffer.len(), buffer.current_bytes)
                    };
                    warn!(
                        "Shared stream subscriber {} lagged behind burst buffer \
                         (skipped={} chunks, ring_occupancy={} chunks/{} bytes); terminating subscriber only",
                        sanitize_sensitive_info(&address.to_string()),
                        read.skipped,
                        ring_chunks,
                        buffered_bytes,
                    );
                    *forwarder_terminal_error_slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(StreamError::SubscriberLagged);
                    // Returning drops client_tx, which closes the client queue and surfaces the
                    // recorded error on the subscriber's next poll.
                    return;
                }

                trace_if_enabled!(
                    "shared_stream.subscribe: read {} chunks (next_seq={}, skipped={}) for {}",
                    read_chunks.len(),
                    read.next_sequence,
                    read.skipped,
                    sanitize_sensitive_info(&address.to_string())
                );

                if !read_chunks.is_empty() {
                    for data in read_chunks.drain(..) {
                        let chunk_len = data.len();
                        match send_client_chunk(
                            &client_tx,
                            data,
                            &cancel_token,
                            last_active + timeout_duration,
                            &byte_budget,
                        )
                        .await
                        {
                            SendOutcome::Sent => {}
                            outcome => {
                                debug!("Shared stream client send error ({outcome:?}): {address}");
                                cancel_token.cancel();
                                cleanup_manager.send_cleanup(CleanupEvent::ReleaseSharedSubscriber {
                                    addr: address,
                                    subscriber_id,
                                    request_id: None,
                                    owner: None,
                                    provider_end_reason: PROVIDER_END_NOT_SET,
                                    provider_error_class: None,
                                });
                                return;
                            }
                        }
                        if !first_live_chunk_logged {
                            debug_if_enabled!(
                                "Shared stream subscriber {} received first live chunk after {} ms",
                                sanitize_sensitive_info(&address.to_string()),
                                subscriber_started_at.elapsed().as_millis()
                            );
                            first_live_chunk_logged = true;
                        }
                        if !startup_stats_logged {
                            startup_chunks_sent = startup_chunks_sent.saturating_add(1);
                            startup_bytes_sent = startup_bytes_sent.saturating_add(chunk_len);
                            if subscriber_started_at.elapsed() >= Duration::from_secs(5) {
                                debug_if_enabled!(
                                    "Shared stream subscriber {} startup throughput: chunks={} bytes={} over {} ms (queue_used={}/{})",
                                    sanitize_sensitive_info(&address.to_string()),
                                    startup_chunks_sent,
                                    startup_bytes_sent,
                                    subscriber_started_at.elapsed().as_millis(),
                                    subscriber_buf_size.saturating_sub(client_tx_clone.capacity()),
                                    subscriber_buf_size
                                );
                                startup_stats_logged = true;
                            }
                        }
                        last_active = Instant::now();
                    }
                    continue;
                }

                tokio::select! {
                    biased;

                    () = cancel_token.cancelled() => {
                        trace_if_enabled!(
                            "shared_stream.subscribe: cancel_received for {}",
                            sanitize_sensitive_info(&address.to_string())
                        );
                        break;
                    }

                    () = &mut idle_check => {
                        if last_active.elapsed() > timeout_duration {
                            trace_if_enabled!(
                                "shared_stream.subscribe: idle_check_fired (inactivity>={}s) for {}",
                                timeout_duration.as_secs(),
                                sanitize_sensitive_info(&address.to_string())
                            );
                            cancel_token.cancel();
                            break;
                        }
                        idle_check.as_mut().reset(Instant::now() + idle_check_interval);
                    }

                    () = notified_fut => {
                        trace_if_enabled!(
                            "shared_stream.subscribe: empty_buffer_waiting waker for {}",
                            sanitize_sensitive_info(&address.to_string())
                        );
                    }

                    () = preempted_token.cancelled() => {
                        trace_if_enabled!(
                            "shared_stream.subscribe: preempted for {}",
                            sanitize_sensitive_info(&address.to_string())
                        );
                        if let Some(mut fallback) = low_priority_preempted {
                            debug_if_enabled!(
                                "Shared stream subscriber {} switching to low_priority_preempted fallback",
                                sanitize_sensitive_info(&address.to_string())
                            );
                            while let Some(chunk) = fallback.next_chunk() {
                                match send_client_chunk(&client_tx, chunk, &cancel_token, last_active + timeout_duration, &byte_budget).await {
                                    SendOutcome::Sent => last_active = Instant::now(),
                                    outcome => {
                                        debug!(
                                            "Shared stream fallback send error ({outcome:?}) for {}",
                                            sanitize_sensitive_info(&address.to_string())
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }

            // A failing origin is classified by the response wrapper after it drains the
            // final burst; an unclassified release here would preempt that classification.
            if !origin_state.terminal_is_failure() || cancel_token.is_cancelled() || preempted_token.is_cancelled() {
                cleanup_manager.send_cleanup(CleanupEvent::ReleaseSharedSubscriber {
                    addr: address,
                    subscriber_id,
                    request_id: None,
                    owner: None,
                    provider_end_reason: PROVIDER_END_NOT_SET,
                    provider_error_class: None,
                });
            }
        });

        self.lock_task_handles().push(handle);

        let provider = self.provider_guard.as_ref().and_then(|h| h.allocation.get_provider_name());
        let (permit, request_id, owner) = pending_cleanup.take_cleanup();
        (
            ReceiverStreamWrapper {
                stream: ReceiverStream::new(client_rx).map(|chunk| chunk.bytes),
                start: Some(start_tx),
                subscriber_id,
                addr: *addr,
                permit,
                request_id,
                owner,
                deadline: self.subscriber_max_duration.map(|duration| Box::pin(sleep(duration))),
                released: false,
                terminal_outcome: Arc::clone(&self.terminal_outcome),
                terminal_observed: false,
                subscriber_terminal_error,
                cancel_token: forwarder_cancel_token,
                preempted_token: self.preempted_token.clone(),
            }
            .boxed(),
            provider,
            SharedCleanupCapability::new(subscriber_id),
        )
    }
}
