use super::{
    PendingJoinGuard, PendingSharedMeterRegistration, PendingSharedSubscriberCleanup, ReceiverStreamWrapper,
    SharedStreamState, TerminalOutcome, DEFAULT_SHARED_BUFFER_SIZE_BYTES,
};
use crate::{
    CleanupEvent, SharedCleanupCapability, PROVIDER_END_ERROR, PROVIDER_END_NOT_SET, PROVIDER_END_SUBSCRIBER_LAGGED,
};
use bytes::Bytes;
use futures::Stream;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tuliprox_core::model::{Config, SharedSubscriberId, StreamError};

impl PendingSharedMeterRegistration {
    pub fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for PendingSharedMeterRegistration {
    fn drop(&mut self) {
        if self.armed {
            self.manager.remove_meter_uid_if_pending(&self.stream_url, self.meter_uid);
        }
    }
}

impl PendingJoinGuard {
    pub(super) fn new(state: &Arc<SharedStreamState>) -> Self {
        state.increment_pending_joins();
        Self { state: Arc::clone(state), armed: true }
    }

    pub(super) fn commit(mut self) {
        self.armed = false;
        self.state.decrement_pending_joins();
    }
}

impl Drop for PendingJoinGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state.decrement_pending_joins();
        }
    }
}

impl PendingSharedSubscriberCleanup {
    pub(super) fn new(
        permit: tokio::sync::mpsc::OwnedPermit<CleanupEvent>,
        subscriber_id: SharedSubscriberId,
        addr: SocketAddr,
    ) -> Self {
        Self { permit: Some(permit), subscriber_id, addr, request_id: None, owner: None }
    }

    pub fn set_provider_request_identity(&mut self, owner: &str, request_id: tuliprox_core::model::PlaybackRequestId) {
        self.owner = Some(Arc::from(owner));
        self.request_id = Some(request_id);
    }

    /// Produces the cleanup-ownership proof for this subscriber. The permit held by
    /// this guard is the source of the capability.
    pub fn capability(&self) -> SharedCleanupCapability {
        SharedCleanupCapability::new(self.subscriber_id)
    }

    pub(super) fn take_cleanup(
        &mut self,
    ) -> (
        Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
        Option<tuliprox_core::model::PlaybackRequestId>,
        Option<Arc<str>>,
    ) {
        (self.permit.take(), self.request_id.take(), self.owner.take())
    }

    pub fn disarm(&mut self) {
        self.permit = None;
    }
}

impl Drop for PendingSharedSubscriberCleanup {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            permit.send(CleanupEvent::ReleaseSharedSubscriber {
                addr: self.addr,
                subscriber_id: self.subscriber_id,
                request_id: self.request_id,
                owner: self.owner.take(),
                provider_end_reason: PROVIDER_END_NOT_SET,
                provider_error_class: None,
            });
        }
    }
}

impl<S> ReceiverStreamWrapper<S> {
    fn peek_subscriber_terminal_error(&self) -> Option<StreamError> {
        self.subscriber_terminal_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub(super) fn release(&mut self) {
        if !self.released {
            self.released = true;
            if let Some(permit) = self.permit.take() {
                let (provider_end_reason, provider_error_class) = if self.terminal_observed
                    && !self.cancel_token.is_cancelled()
                    && !self.preempted_token.is_cancelled()
                {
                    // A subscriber-local terminal error (e.g. lag) wins over the origin's
                    // outcome: it is what this response actually delivered.
                    if let Some(local_err) = self.peek_subscriber_terminal_error() {
                        if matches!(local_err, StreamError::SubscriberLagged) {
                            (PROVIDER_END_SUBSCRIBER_LAGGED, Some(local_err.provider_error_class()))
                        } else {
                            (PROVIDER_END_ERROR, Some(local_err.provider_error_class()))
                        }
                    } else {
                        self.terminal_outcome
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_ref()
                            .map_or((PROVIDER_END_NOT_SET, None), TerminalOutcome::cleanup_classification)
                    }
                } else {
                    (PROVIDER_END_NOT_SET, None)
                };
                permit.send(CleanupEvent::ReleaseSharedSubscriber {
                    addr: self.addr,
                    subscriber_id: self.subscriber_id,
                    request_id: self.request_id,
                    owner: self.owner.take(),
                    provider_end_reason,
                    provider_error_class,
                });
            }
        }
    }
}

impl<S> Stream for ReceiverStreamWrapper<S>
where
    S: Stream<Item = Bytes> + Unpin,
{
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.deadline.as_mut().is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready()) {
            self.release();
            return Poll::Ready(None);
        }
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(None) => {
                // Inner queue closed. A subscriber-local error (lag) takes precedence, then
                // the origin's terminal outcome. Either way the client sees the cause before
                // the reserved cleanup is classified.
                if !self.terminal_observed && !self.cancel_token.is_cancelled() && !self.preempted_token.is_cancelled()
                {
                    if let Some(local_err) = self.peek_subscriber_terminal_error() {
                        self.terminal_observed = true;
                        return Poll::Ready(Some(Err(local_err)));
                    }
                    let terminal_err = self
                        .terminal_outcome
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .and_then(TerminalOutcome::as_stream_error);
                    if let Some(err) = terminal_err {
                        self.terminal_observed = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                self.release();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(super) fn resolve_min_burst_buffer_bytes(config: &Config) -> usize {
    config
        .reverse_proxy
        .as_ref()
        .and_then(|rp| rp.stream.as_ref())
        .and_then(|stream| usize::try_from(stream.shared_burst_buffer_mb.saturating_mul(1024 * 1024)).ok())
        .unwrap_or(DEFAULT_SHARED_BUFFER_SIZE_BYTES)
        .max(1)
}

impl<S> Drop for ReceiverStreamWrapper<S> {
    fn drop(&mut self) {
        self.release();
    }
}
