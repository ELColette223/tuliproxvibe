use crate::{ActiveProviderManager, CleanupEvent, PROVIDER_END_CLOSED, PROVIDER_END_ERROR};
use bytes::Bytes;
use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    net::SocketAddr,
    pin::Pin,
    sync::{atomic::AtomicUsize, Arc},
};
use tokio::{
    sync::{oneshot, Mutex, Notify, RwLock},
    time::{Duration, Sleep},
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{AllocationId, ProviderHandle, SharedSubscriberId, StreamError};

const DEFAULT_SHARED_BUFFER_SIZE_BYTES: usize = 1024 * 1024 * 32;

const YIELD_COUNTER: usize = 64;

const MIN_BURST_BUFFER_CHUNKS: usize = 2;

const MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES: usize = 188;

const SHARED_BURST_BYTES_PER_BUFFER_SLOT: usize = 12 * 1024;

const DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS: u64 = 300;

const SHARED_CLEANUP_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

// Upper bound on chunks pulled into a subscriber's live batch per read. The burst ring
// is byte-bounded already; this caps how many of its entries are cloned into the
// per-subscriber vector at once so a long backlog cannot balloon that vector.
const MAX_SUBSCRIBER_LIVE_BATCH_CHUNKS: usize = 64;

/// Typed terminal outcome of a shared origin, replacing the old boolean flag.
/// Stored once by the broadcaster when the provider body ends or errors; each
/// subscriber drains remaining burst data and then propagates this outcome.
#[derive(Debug, Clone)]
enum TerminalOutcome {
    /// Provider body ended normally (EOF).
    ProviderEof,
    /// Provider body produced a terminal `StreamError`.
    ProviderError(StreamError),
}

impl TerminalOutcome {
    /// The `StreamError` to deliver to each subscriber, if any.
    fn as_stream_error(&self) -> Option<StreamError> {
        match self {
            Self::ProviderEof => None,
            Self::ProviderError(e) => Some(e.clone()),
        }
    }

    /// Classification constants for the cleanup event.
    fn cleanup_classification(&self) -> (u8, Option<&'static str>) {
        match self {
            Self::ProviderEof => (PROVIDER_END_CLOSED, None),
            Self::ProviderError(e) => (PROVIDER_END_ERROR, Some(e.provider_error_class())),
        }
    }

    /// Whether this outcome represents a provider failure (not a clean EOF).
    fn is_failure(&self) -> bool { matches!(self, Self::ProviderError(_)) }
}

pub struct PendingSharedSubscriberCleanup {
    permit: Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
    subscriber_id: SharedSubscriberId,
    addr: SocketAddr,
    request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    owner: Option<Arc<str>>,
}

pub struct PendingSharedMeterRegistration {
    manager: Arc<SharedStreamManager>,
    stream_url: Arc<str>,
    meter_uid: u32,
    armed: bool,
}

/// Tracks a shared-stream meter reservation. `owner` is `None` while the meter is
/// only reserved; it becomes `Some(allocation_id)` once a live shared origin adopts it.
/// Pending rollbacks may only remove an entry that no origin has adopted yet.
struct SharedMeterEntry {
    uid: u32,
    owner: Option<AllocationId>,
}

/// RAII guard for a join that committed its registry entry but has not yet registered
/// its subscriber. It decrements the origin's pending-join counter exactly once, either
/// on successful registration (`commit`) or when the join future is dropped.
struct PendingJoinGuard {
    state: Arc<SharedStreamState>,
    armed: bool,
}

struct ReceiverStreamWrapper<S> {
    stream: S,
    start: Option<oneshot::Sender<()>>,
    subscriber_id: SharedSubscriberId,
    addr: SocketAddr,
    /// Guaranteed cleanup right reserved before subscriber registration, so the
    /// release below is never dropped by cleanup-queue pressure.
    permit: Option<tokio::sync::mpsc::OwnedPermit<CleanupEvent>>,
    request_id: Option<tuliprox_core::model::PlaybackRequestId>,
    owner: Option<Arc<str>>,
    deadline: Option<Pin<Box<Sleep>>>,
    released: bool,
    terminal_outcome: Arc<std::sync::Mutex<Option<TerminalOutcome>>>,
    terminal_observed: bool,
    /// Subscriber-local terminal error slot (e.g. `SubscriberLagged`), shared with the
    /// forwarder task. When the forwarder detects lag it records the error here instead
    /// of cancelling the subscriber, so the wrapper can deliver the error to the client
    /// and only then classify its reserved cleanup. Takes precedence over the origin's
    /// terminal outcome.
    subscriber_terminal_error: Arc<std::sync::Mutex<Option<StreamError>>>,
    cancel_token: CancellationToken,
    preempted_token: CancellationToken,
}

#[derive(Clone, Debug)]
struct SharedSubscriber {
    addr: SocketAddr,
    cancel_token: CancellationToken,
}

struct BufferedChunk {
    sequence: u64,
    bytes: Bytes,
}

struct BurstBuffer {
    buffer: VecDeque<BufferedChunk>,
    buffer_size: usize,
    max_chunks: usize,
    current_bytes: usize,
    next_sequence: u64,
}

struct BurstRead {
    next_sequence: u64,
    skipped: u64,
}

/// A queue entry that releases its byte-budget permit when the client consumes (drops) it.
struct BudgetedChunk {
    bytes: Bytes,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Debug)]
pub struct SharedStreamState {
    headers: Vec<(String, String)>,
    buf_size: usize,
    provider_guard: Option<ProviderHandle>,
    low_priority_preempted: Option<tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer>,
    preempted_token: CancellationToken,
    subscribers: RwLock<HashMap<SubscriberId, SharedSubscriber>>,
    stop_token: CancellationToken,
    burst_buffer: Arc<Mutex<BurstBuffer>>,
    live_notification: Arc<Notify>,
    terminal_outcome: Arc<std::sync::Mutex<Option<TerminalOutcome>>>,
    task_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    subscriber_idle_timeout_secs: u64,
    subscriber_max_duration: Option<Duration>,
    /// Number of joins that committed their registry entry but have not yet registered
    /// their subscriber in `subscribers`. Teardown must not remove an origin while this is
    /// non-zero, otherwise a join in flight would land on a stopped state.
    pending_joins: AtomicUsize,
}

#[derive(Debug, Clone, Default)]
struct SharedStreamsRegister {
    by_key: HashMap<Arc<str>, Arc<SharedStreamState>>,
    key_by_subscriber: HashMap<SubscriberId, Arc<str>>,
}

pub struct SharedStreamManager {
    provider_manager: Arc<ActiveProviderManager>,
    shared_streams: RwLock<SharedStreamsRegister>,
    meter_uids: std::sync::Mutex<HashMap<Arc<str>, SharedMeterEntry>>,
    #[cfg(test)]
    pub(crate) test_preflight_barrier: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
}

#[cfg(test)]
mod tests;

mod broadcast;
mod buffer;
mod manager;
mod reader;
mod subscription;
use buffer::SubscriberId;
pub use manager::SharedStreamCtx;
