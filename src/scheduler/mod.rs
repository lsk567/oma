pub mod event;
mod pane_input;

pub use event::ScheduledEvent;

use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

use crate::ea;
use crate::process::pid_file_is_stale;
use crate::tmux::DeliveryOptions;

/// A ticker message with its creation time.
struct TickerEntry {
    text: String,
    created_at: Instant,
}

/// Thread-safe scrolling ticker buffer shared between scheduler and UI.
#[derive(Clone)]
pub struct TickerBuffer {
    entries: Arc<Mutex<VecDeque<TickerEntry>>>,
}

impl TickerBuffer {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Push a new message into the ticker. Caps at 50 entries.
    pub fn push(&self, msg: impl Into<String>) {
        let mut buf = self.entries.lock().unwrap();
        if buf.len() >= 50 {
            buf.pop_front();
        }
        buf.push_back(TickerEntry {
            text: msg.into(),
            created_at: Instant::now(),
        });
    }

    /// Return the joined ticker content, filtering entries older than `ttl`.
    /// Does NOT prune the buffer — old entries remain for `latest()` / debug console.
    pub fn render(&self, ttl: std::time::Duration) -> String {
        let buf = self.entries.lock().unwrap();
        let now = Instant::now();
        buf.iter()
            .filter(|e| now.duration_since(e.created_at) < ttl)
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join(" +++ ")
    }

    /// Return the last `n` messages regardless of age (for debug console).
    pub fn latest(&self, n: usize) -> Vec<String> {
        let buf = self.entries.lock().unwrap();
        buf.iter()
            .rev()
            .take(n)
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_nanos() as u64
}

/// Combine an optional `seconds` and optional `extra_ns` duration into a
/// single nanosecond count, saturating on overflow. Returns `None` only
/// when both inputs are absent — callers that want a default (e.g. "no
/// delay means 0") should `.unwrap_or(0)`. Shared between the CLI and
/// MCP event-scheduling paths so the arithmetic has one definition.
pub fn combine_seconds_and_ns(seconds: Option<u64>, extra_ns: Option<u64>) -> Option<u64> {
    match (seconds, extra_ns) {
        (Some(s), Some(n)) => Some(s.saturating_mul(1_000_000_000).saturating_add(n)),
        (Some(s), None) => Some(s.saturating_mul(1_000_000_000)),
        (None, Some(n)) => Some(n),
        (None, None) => None,
    }
}

pub struct Scheduler {
    queue: Mutex<BinaryHeap<ScheduledEvent>>,
    notify: Notify,
    store_path: Option<PathBuf>,
}

struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    /// Try to acquire the cross-process scheduler lock. The retry budget is
    /// kept short on purpose: `transaction()` is reachable from async event
    /// loops (e.g. `take_due_deliveries`), so a long blocking wait here would
    /// stall a Tokio worker thread. Lock callers degrade gracefully to
    /// in-memory operation on failure (see `Scheduler::transaction`), so
    /// timing out fast is preferable to long blocking.
    fn acquire(path: PathBuf) -> std::io::Result<Self> {
        const MAX_ATTEMPTS: u32 = 50;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);
        for _ in 0..MAX_ATTEMPTS {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if pid_file_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(err) => return Err(err),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("Timed out waiting for scheduler lock {:?}", path),
        ))
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn events_store_path(omar_dir: &Path) -> PathBuf {
    omar_dir.join("scheduled_events.json")
}

fn store_lock_path(store_path: &Path) -> PathBuf {
    store_path.with_extension("lock")
}

fn load_events_from_store(store_path: &Path) -> Vec<ScheduledEvent> {
    fs::read_to_string(store_path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_events_to_store(store_path: &Path, queue: &BinaryHeap<ScheduledEvent>) {
    let mut events: Vec<ScheduledEvent> = queue.iter().cloned().collect();
    events.sort_by_key(|event| (event.timestamp, event.created_at));
    if let Some(parent) = store_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = store_path.with_extension("tmp");
    if let Ok(content) = serde_json::to_string_pretty(&events) {
        if fs::write(&tmp, content).is_ok() {
            let _ = fs::rename(&tmp, store_path);
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }
}

impl Scheduler {
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(BinaryHeap::new()),
            notify: Notify::new(),
            store_path: None,
        }
    }

    pub fn with_store(store_path: PathBuf) -> Self {
        let mut queue = BinaryHeap::new();
        for event in load_events_from_store(&store_path) {
            queue.push(event);
        }
        Self {
            queue: Mutex::new(queue),
            notify: Notify::new(),
            store_path: Some(store_path),
        }
    }

    pub fn is_persistent(&self) -> bool {
        self.store_path.is_some()
    }

    fn transaction<R>(
        &self,
        persist: bool,
        f: impl FnOnce(&mut BinaryHeap<ScheduledEvent>) -> R,
    ) -> R {
        match &self.store_path {
            Some(store_path) => {
                // If the on-disk lock can't be acquired (stuck peer, slow FS),
                // degrade to an in-memory operation rather than panicking the
                // caller — the event loop or any other thread holding a live
                // Scheduler would otherwise take down the whole process.
                // Consequence on a write op: the change only lands in the
                // in-memory heap and will be overwritten by the next
                // successful `transaction` (which reloads from disk), so the
                // event is effectively lost if the dashboard restarts or
                // another writer succeeds first. Still preferable to a crash.
                let _store_lock = match StoreLock::acquire(store_lock_path(store_path)) {
                    Ok(lock) => lock,
                    Err(err) => {
                        if persist {
                            eprintln!(
                                "scheduler: WARNING: failed to acquire store lock ({}); \
                                 write applied to in-memory cache only — NOT persisted to disk \
                                 and may be lost on restart",
                                err
                            );
                        } else {
                            eprintln!(
                                "scheduler: failed to acquire store lock ({}); \
                                 read returned from (potentially stale) in-memory cache",
                                err
                            );
                        }
                        let mut queue = self.queue.lock().unwrap();
                        return f(&mut queue);
                    }
                };
                let mut queue = BinaryHeap::new();
                for event in load_events_from_store(store_path) {
                    queue.push(event);
                }
                let result = f(&mut queue);
                if persist {
                    save_events_to_store(store_path, &queue);
                }
                *self.queue.lock().unwrap() = queue;
                result
            }
            None => {
                let mut queue = self.queue.lock().unwrap();
                f(&mut queue)
            }
        }
    }

    pub fn insert(&self, event: ScheduledEvent) {
        self.transaction(true, |queue| {
            queue.push(event);
        });
        // `notify_one` (not `notify_waiters`) stores a permit so the event
        // loop wakes up even if it isn't currently parked on
        // `notified().await`. All queue mutations follow this convention so
        // a cancellation that removed the next-due event can't leave the
        // loop sleeping until the stale deadline.
        self.notify.notify_one();
    }

    /// Cancel an event only if it belongs to the specified EA.
    /// Fix S1: Atomic EA-scoped cancellation — no TOCTOU window where the event
    /// is temporarily absent from the queue (as happens with cancel + re-insert).
    /// Returns:
    ///   Ok(event) if found and ea_id matches (event removed)
    ///   Err(true)  if found but ea_id doesn't match (event stays in queue)
    ///   Err(false) if not found
    pub fn cancel_if_ea(&self, event_id: &str, ea_id: u32) -> Result<ScheduledEvent, bool> {
        let result = self.transaction(true, |queue| {
            let events: Vec<ScheduledEvent> = queue.drain().collect();
            let mut cancelled = None;
            let mut wrong_ea = false;
            let mut remaining = BinaryHeap::new();
            for ev in events {
                if ev.id == event_id && cancelled.is_none() {
                    if ev.ea_id == ea_id {
                        cancelled = Some(ev);
                    } else {
                        wrong_ea = true;
                        remaining.push(ev);
                    }
                } else {
                    remaining.push(ev);
                }
            }
            *queue = remaining;
            match cancelled {
                Some(ev) => Ok(ev),
                None => Err(wrong_ea),
            }
        });
        self.notify.notify_one();
        result
    }

    /// List events for a specific EA only.
    pub fn list_by_ea(&self, ea_id: u32) -> Vec<ScheduledEvent> {
        self.transaction(false, |queue| {
            queue.iter().filter(|e| e.ea_id == ea_id).cloned().collect()
        })
    }

    /// Cancel all events for a specific EA. Returns the number cancelled.
    pub fn cancel_by_ea(&self, ea_id: u32) -> usize {
        let count = self.transaction(true, |queue| {
            let events: Vec<ScheduledEvent> = queue.drain().collect();
            let mut count = 0;
            let mut remaining = BinaryHeap::new();
            for ev in events {
                if ev.ea_id == ea_id {
                    count += 1;
                } else {
                    remaining.push(ev);
                }
            }
            *queue = remaining;
            count
        });
        self.notify.notify_one();
        count
    }

    /// Cancel all events for a given receiver within a specific EA only.
    /// Fix V5: EA-scoped receiver cancellation prevents cross-EA event leaks.
    pub fn cancel_by_receiver_and_ea(&self, receiver: &str, ea_id: u32) -> usize {
        let count = self.transaction(true, |queue| {
            let events: Vec<ScheduledEvent> = queue.drain().collect();
            let mut count = 0;
            let mut remaining = BinaryHeap::new();
            for ev in events {
                if ev.receiver == receiver && ev.ea_id == ea_id {
                    count += 1;
                } else {
                    remaining.push(ev);
                }
            }
            *queue = remaining;
            count
        });
        self.notify.notify_one();
        count
    }

    /// Pop all events matching the given receiver, EA, and timestamp.
    /// Fix V7: EA-scoped batching prevents cross-EA event delivery.
    #[cfg(test)]
    pub fn pop_batch(&self, receiver: &str, ea_id: u32, timestamp: u64) -> Vec<ScheduledEvent> {
        let batch = self.transaction(true, |queue| {
            let events: Vec<ScheduledEvent> = queue.drain().collect();
            let mut batch = Vec::new();
            let mut remaining = BinaryHeap::new();
            for ev in events {
                if ev.receiver == receiver && ev.ea_id == ea_id && ev.timestamp == timestamp {
                    batch.push(ev);
                } else {
                    remaining.push(ev);
                }
            }
            *queue = remaining;
            batch
        });
        self.notify.notify_one();
        batch
    }

    fn next_timestamp(&self) -> Option<u64> {
        self.transaction(false, |queue| queue.peek().map(|event| event.timestamp))
    }
    fn take_due_deliveries(
        &self,
        popup_receiver: &PopupReceiver,
        base_prefix: &str,
    ) -> Vec<DueDelivery> {
        self.transaction(true, |queue| {
            let Some(earliest_ts) = queue.peek().map(|event| event.timestamp) else {
                return Vec::new();
            };
            if earliest_ts > now_ns() {
                return Vec::new();
            }

            let mut remaining = BinaryHeap::new();
            let mut groups: Vec<((String, u32), Vec<ScheduledEvent>)> = Vec::new();
            let mut group_indices: HashMap<(String, u32), usize> = HashMap::new();

            for event in queue.drain() {
                if event.timestamp == earliest_ts {
                    let key = (event.receiver.clone(), event.ea_id);
                    let idx = *group_indices.entry(key.clone()).or_insert_with(|| {
                        groups.push((key, Vec::new()));
                        groups.len() - 1
                    });
                    groups[idx].1.push(event);
                } else {
                    remaining.push(event);
                }
            }

            const MAX_EVENTS_PER_EA_PER_TICK: usize = 10;
            let mut ea_delivery_count: HashMap<u32, usize> = HashMap::new();
            let mut deliveries = Vec::new();

            for ((receiver, ea_id), batch) in groups {
                let delivered_so_far = *ea_delivery_count.get(&ea_id).unwrap_or(&0);
                if delivered_so_far >= MAX_EVENTS_PER_EA_PER_TICK {
                    for event in batch {
                        remaining.push(event);
                    }
                    continue;
                }

                let mut restore_input = None;
                // A side channel hands the event straight to the backend, so
                // the composer is never involved: nothing to capture, nothing
                // to clear, and no reason to defer around a popup.
                if should_defer_for_popup(popup_receiver, &receiver, ea_id)
                    && side_channel(&receiver, ea_id, base_prefix).is_none()
                {
                    // Empty the box so the event does not land on top of what
                    // the user is typing, and hold their draft to put back
                    // afterwards. A pane that cannot be read or cannot be
                    // cleared defers instead: a late event is recoverable, a
                    // destroyed draft is not.
                    match protect_draft(base_prefix, &receiver, ea_id) {
                        Some(draft) => restore_input = draft,
                        None => {
                            defer_batch(
                                batch,
                                &mut remaining,
                                &mut deliveries,
                                receiver,
                                ea_id,
                                earliest_ts,
                            );
                            continue;
                        }
                    }
                }

                let remaining_quota = MAX_EVENTS_PER_EA_PER_TICK - delivered_so_far;
                let (batch, deferred_batch) = split_batch_for_quota(batch, remaining_quota);
                for event in deferred_batch {
                    remaining.push(event);
                }
                if batch.is_empty() {
                    continue;
                }

                *ea_delivery_count.entry(ea_id).or_insert(0) += batch.len();
                for event in &batch {
                    if let Some(interval) = event.recurring_ns {
                        let now = now_ns();
                        remaining.push(ScheduledEvent {
                            id: uuid::Uuid::new_v4().to_string(),
                            sender: event.sender.clone(),
                            receiver: event.receiver.clone(),
                            timestamp: now + interval,
                            payload: event.payload.clone(),
                            created_at: now,
                            recurring_ns: Some(interval),
                            ea_id: event.ea_id,
                        });
                    }
                }
                deliveries.push(DueDelivery {
                    receiver,
                    ea_id,
                    timestamp: earliest_ts,
                    batch,
                    deferred_for_popup: false,
                    restore_input,
                });
            }

            *queue = remaining;
            deliveries
        })
    }
}

/// Push a batch back onto the queue to be retried after the popup delay.
///
/// Used whenever the pane cannot be read or cannot be cleared: a late event is
/// recoverable, a destroyed draft is not.
fn defer_batch(
    batch: Vec<ScheduledEvent>,
    remaining: &mut BinaryHeap<ScheduledEvent>,
    deliveries: &mut Vec<DueDelivery>,
    receiver: String,
    ea_id: ea::EaId,
    earliest_ts: u64,
) {
    let defer_until = now_ns() + POPUP_DEFER_NS;
    for mut event in batch {
        event.timestamp = defer_until;
        remaining.push(event);
    }
    deliveries.push(DueDelivery {
        receiver,
        ea_id,
        timestamp: earliest_ts,
        batch: Vec::new(),
        deferred_for_popup: true,
        restore_input: None,
    });
}

/// Build the tmux session name for a receiver.
fn pane_target_name(receiver: &str, ea_id: ea::EaId, base_prefix: &str) -> String {
    if receiver == "ea" || receiver == "omar" {
        ea::ea_manager_session(ea_id, base_prefix)
    } else {
        format!("{}{}", ea::ea_prefix(ea_id, base_prefix), receiver)
    }
}

/// Capture the user's in-progress draft from an agent pane.
///
/// The backend is read from the session stamp written at launch rather than
/// sniffed from the pane, and the capture is the visible pane only, so
/// scrollback cannot contribute stale prompt rows.
fn get_pane_input(base_prefix: &str, receiver: &str, ea_id: ea::EaId) -> pane_input::PaneInput {
    let target = pane_target_name(receiver, ea_id, base_prefix);
    let client = crate::tmux::TmuxClient::new("");

    let Some(backend) = client.session_backend(&target) else {
        return pane_input::PaneInput::Unknown("backend not identified");
    };
    let Some(shape) = pane_input::Shape::for_backend(&backend) else {
        return pane_input::PaneInput::Unknown("backend has no known input shape");
    };
    let Ok(capture) = client.capture_pane_visible(&target) else {
        return pane_input::PaneInput::Unknown("pane capture failed");
    };

    let caret = client
        .caret_position(&target)
        .map(|(row, col)| pane_input::Caret { row, col });
    let width = client.pane_width(&target);
    pane_input::extract(shape, &capture, caret, width)
}

/// Empty the input box, confirming it by re-reading the pane.
///
/// `C-u` kills one line at a time — a visual line on some backends, a logical
/// one on others — so the count needed varies with the draft. Rather than
/// guess, press and re-check until the box reads empty. Returns `false` if it
/// never does, so the caller can put the draft back and defer.
/// What emptying the input box achieved.
#[derive(Debug, PartialEq, Eq)]
enum Cleared {
    /// The box is empty.
    Empty,
    /// Nothing was sent, so whatever was there is untouched.
    Untouched,
    /// Keys were sent and the box still holds text — it is damaged now.
    Partial,
}

fn clear_pane_input(base_prefix: &str, receiver: &str, ea_id: ea::EaId, target: &str) -> Cleared {
    const MAX_CLEAR_PRESSES: usize = 40;
    let client = crate::tmux::TmuxClient::new("");
    let mut pressed = false;

    for _ in 0..MAX_CLEAR_PRESSES {
        match get_pane_input(base_prefix, receiver, ea_id) {
            pane_input::PaneInput::Empty => return Cleared::Empty,
            // Never keep hammering a pane we cannot read. Whether the draft is
            // still whole depends on how far we got, and the caller needs to
            // know: putting it back on top of an intact draft duplicates it.
            pane_input::PaneInput::Unknown(_) => {
                return if pressed {
                    Cleared::Partial
                } else {
                    Cleared::Untouched
                };
            }
            pane_input::PaneInput::Draft(_) => {}
        }
        let _ = client.send_keys(target, "C-u");
        pressed = true;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    match get_pane_input(base_prefix, receiver, ea_id) {
        pane_input::PaneInput::Empty => Cleared::Empty,
        _ => Cleared::Partial,
    }
}

/// Empty the input box, handing back whatever was in it so it can be put
/// back afterwards. `None` means the pane could not be read or could not be
/// cleared, and must not be typed into.
fn protect_draft(base_prefix: &str, receiver: &str, ea_id: ea::EaId) -> Option<Option<String>> {
    let draft = match get_pane_input(base_prefix, receiver, ea_id) {
        pane_input::PaneInput::Empty => None,
        pane_input::PaneInput::Draft(draft) => Some(draft),
        // Unreadable: clearing what we cannot put back destroys the draft.
        pane_input::PaneInput::Unknown(_) => return None,
    };

    let target = pane_target_name(receiver, ea_id, base_prefix);
    match clear_pane_input(base_prefix, receiver, ea_id, &target) {
        Cleared::Empty => Some(draft.filter(|draft| !draft.trim().is_empty())),
        // Nothing was sent, so the draft is still whole. Pasting it back here
        // would leave the user with two copies of what they were writing.
        Cleared::Untouched => None,
        Cleared::Partial => {
            // Keys landed and the box still holds part of the draft. Put back
            // what was taken, or the clearing itself is the damage.
            if let Some(draft) = &draft {
                let _ = crate::tmux::TmuxClient::new("").paste_text(&target, draft);
            }
            None
        }
    }
}

/// The side channel for a receiver's pane, if its backend offers one.
fn side_channel(
    receiver: &str,
    ea_id: ea::EaId,
    base_prefix: &str,
) -> Option<crate::channel::Channel> {
    let target = pane_target_name(receiver, ea_id, base_prefix);
    let client = crate::tmux::TmuxClient::new("");
    let backend = client.session_backend(&target)?;
    let pane_pid = client.get_pane_pid(&target).ok()?;
    let stamp = client.session_delivery(&target);
    crate::channel::Channel::resolve(&backend, pane_pid, stamp.as_deref())
}

pub(crate) fn deliver_to_tmux(
    ea_id: u32,
    receiver: &str,
    message: &str,
    base_prefix: &str,
    ticker: &TickerBuffer,
    restore_input: Option<&str>,
) -> bool {
    // Set when the input box had to be emptied on the fallback path.
    let mut rescued = None;

    // Prefer handing the event to the backend directly. It reaches the model
    // without going through the input box, so a draft the user is typing is
    // never touched and the event does not read as something they said.
    if let Some(channel) = side_channel(receiver, ea_id, base_prefix) {
        match channel.deliver(message) {
            Ok(()) => {
                // A channel can appear between capture and delivery — a spool
                // drained, a port finished provisioning — in which case the
                // caller has already emptied the box and is holding the draft.
                // Returning without putting it back would destroy it.
                if let Some(draft) = restore_input.filter(|draft| !draft.is_empty()) {
                    let target = pane_target_name(receiver, ea_id, base_prefix);
                    if crate::tmux::TmuxClient::new("")
                        .paste_text(&target, draft)
                        .is_err()
                    {
                        ticker.push(format!("tmux input restore failed for {}", receiver));
                    }
                }
                ticker.push(format!(
                    "delivered event(s) to {} via {}",
                    receiver,
                    channel.describe()
                ));
                return true;
            }
            Err(e) => {
                ticker.push(format!(
                    "side channel for {} failed ({}); using the input box",
                    receiver, e
                ));
                // The caller skipped its capture because a channel looked
                // available, so anything the user was typing is still in the
                // box and nothing has been cleared. Do that here, or typing
                // the event would land on top of their draft.
                match protect_draft(base_prefix, receiver, ea_id) {
                    Some(draft) => rescued = draft,
                    None => {
                        // Unreadable or unclearable: leave the pane alone.
                        // Losing one event beats destroying a draft.
                        ticker.push(format!(
                            "could not clear the input box for {}; retrying later",
                            receiver
                        ));
                        return false;
                    }
                }
            }
        }
    }

    let target = pane_target_name(receiver, ea_id, base_prefix);
    let client = crate::tmux::TmuxClient::new("");
    let opts = DeliveryOptions::default();
    if let Err(e) = client.deliver_prompt(&target, message, &opts) {
        ticker.push(format!("tmux prompt delivery failed for {}: {}", target, e));
        return false;
    }
    let restore_input = restore_input.or(rescued.as_deref());
    if let Some(input) = restore_input.filter(|input| !input.is_empty()) {
        if let Err(e) = client.paste_text(&target, input) {
            ticker.push(format!("tmux input restore failed for {}: {}", target, e));
            // The event itself did land; only putting the draft back failed.
            return true;
        }
        ticker.push(format!("restored draft input for {}", receiver));
    }
    ticker.push(format!("delivered event(s) to {}", receiver));
    true
}

fn format_delivery(events: &[ScheduledEvent], timestamp: u64) -> String {
    if events.len() == 1 {
        let ev = &events[0];
        let tag = if ev.recurring_ns.is_some() {
            "CRON"
        } else {
            "EVENT"
        };
        format!(
            "[{} at t={}]\nFrom {}: {}",
            tag, timestamp, ev.sender, ev.payload
        )
    } else {
        let mut msg = format!("[EVENT BATCH at t={}]", timestamp);
        for ev in events {
            let tag = if ev.recurring_ns.is_some() {
                "CRON"
            } else {
                "EVENT"
            };
            msg.push_str(&format!("\n[{}] From {}: {}", tag, ev.sender, ev.payload));
        }
        msg
    }
}

fn split_batch_for_quota(
    mut batch: Vec<ScheduledEvent>,
    remaining_quota: usize,
) -> (Vec<ScheduledEvent>, Vec<ScheduledEvent>) {
    if batch.is_empty() || remaining_quota >= batch.len() {
        return (batch, Vec::new());
    }

    batch.sort_by_key(|ev| ev.created_at);
    let deferred = batch.split_off(remaining_quota);
    (batch, deferred)
}

/// Shared state: the (short name, ea_id) of the agent whose popup is currently open, if any.
/// Both fields are required so suppression is scoped per-EA and does not affect same-named
/// agents in other EAs.
pub type PopupReceiver = Arc<Mutex<Option<(String, ea::EaId)>>>;

pub fn new_popup_receiver() -> PopupReceiver {
    Arc::new(Mutex::new(None))
}

/// How long to push delivery out while the user's popup is open.
/// If the popup stays open past this, the loop simply defers again —
/// no unbounded fan-out, one event stays one event.
pub(crate) const POPUP_DEFER_NS: u64 = 30_000_000_000;

struct DueDelivery {
    receiver: String,
    ea_id: u32,
    timestamp: u64,
    batch: Vec<ScheduledEvent>,
    deferred_for_popup: bool,
    restore_input: Option<String>,
}

/// Decide whether to defer an event for `(receiver, ea_id)` because the user
/// has an agent popup open on that exact pane. Scoped per-EA so same-named
/// workers in other EAs deliver as normal.
///
/// `deliver_to_tmux` treats the EA pane as either `"ea"` or `"omar"`, so both
/// forms are accepted as aliases: if the popup is on the EA pane, any event
/// addressed to that EA manager (via either alias) is deferred.
pub(crate) fn should_defer_for_popup(
    popup_receiver: &PopupReceiver,
    receiver: &str,
    ea_id: ea::EaId,
) -> bool {
    popup_receiver
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(r, eid)| *eid == ea_id && receivers_match(r, receiver))
}

/// True when `popup` (the identifier stored when the popup opened) refers to
/// the same receiver pane as `event_receiver`.
///
/// `deliver_to_tmux` treats both `"ea"` and `"omar"` as names for the EA
/// manager pane. `selected_popup_receiver_name` only ever stores `"ea"` for
/// that pane (never `"omar"`), so the alias is intentionally *one-way*:
/// a popup on the EA pane (`"ea"`) must cover events addressed as either
/// `"ea"` or `"omar"`, but a worker short name that happens to be `"omar"`
/// must NOT defer events addressed to the EA.
fn receivers_match(popup: &str, event_receiver: &str) -> bool {
    popup == event_receiver || (popup == "ea" && event_receiver == "omar")
}

pub async fn run_event_loop(
    scheduler: Arc<Scheduler>,
    ticker: TickerBuffer,
    popup_receiver: PopupReceiver,
    base_prefix: String,
) {
    let external_poll_interval = std::time::Duration::from_millis(500);
    loop {
        let next_ts = scheduler.next_timestamp();

        match next_ts {
            None => {
                if scheduler.is_persistent() {
                    tokio::time::sleep(external_poll_interval).await;
                } else {
                    scheduler.notify.notified().await;
                }
                continue;
            }
            Some(ts) => {
                let now = now_ns();
                if ts > now {
                    let sleep_ns = ts - now;
                    let mut duration = std::time::Duration::from_nanos(sleep_ns);
                    if scheduler.is_persistent() {
                        duration = duration.min(external_poll_interval);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(duration) => {
                            continue;
                        }
                        _ = scheduler.notify.notified() => {
                            continue;
                        }
                    }
                }

                for delivery in scheduler.take_due_deliveries(&popup_receiver, &base_prefix) {
                    let DueDelivery {
                        receiver,
                        ea_id,
                        timestamp,
                        batch,
                        deferred_for_popup,
                        restore_input,
                    } = delivery;
                    if deferred_for_popup {
                        ticker.push(format!("deferred event(s) for {} (popup open)", receiver));
                        continue;
                    }
                    if batch.is_empty() {
                        continue;
                    }

                    let message = format_delivery(&batch, timestamp);
                    let receiver_name = receiver.clone();
                    let base_prefix_clone = base_prefix.clone();
                    let ticker_clone = ticker.clone();
                    let delivery_result = tokio::task::spawn_blocking(move || {
                        deliver_to_tmux(
                            ea_id,
                            &receiver_name,
                            &message,
                            &base_prefix_clone,
                            &ticker_clone,
                            restore_input.as_deref(),
                        )
                    })
                    .await;
                    // The batch has already left the queue. If it never
                    // reached the agent, put it back rather than lose it.
                    if matches!(delivery_result, Ok(false)) {
                        let retry_at = now_ns() + POPUP_DEFER_NS;
                        for mut event in batch {
                            event.timestamp = retry_at;
                            scheduler.insert(event);
                        }
                        continue;
                    }
                    if let Err(e) = delivery_result {
                        ticker.push(format!(
                            "delivery task failed for {} (ea {}): {}",
                            receiver, ea_id, e
                        ));
                    }

                    let lag_ns = now_ns().saturating_sub(timestamp);
                    let lag_ms = lag_ns as f64 / 1_000_000.0;
                    ticker.push(format!(
                        "delivered {} event(s) to {}, lag={:.2}ms",
                        batch.len(),
                        receiver,
                        lag_ms
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(receiver: &str, sender: &str, timestamp: u64, payload: &str) -> ScheduledEvent {
        ScheduledEvent {
            id: uuid::Uuid::new_v4().to_string(),
            sender: sender.to_string(),
            receiver: receiver.to_string(),
            timestamp,
            payload: payload.to_string(),
            created_at: now_ns(),
            recurring_ns: None,
            ea_id: 0,
        }
    }

    #[test]
    fn test_insert_and_peek() {
        let sched = Scheduler::new();
        sched.insert(make_event("bob", "alice", 200, "hello"));
        sched.insert(make_event("bob", "alice", 100, "earlier"));

        let min_ts = sched.list_by_ea(0).iter().map(|e| e.timestamp).min();
        assert_eq!(min_ts, Some(100));
    }

    #[test]
    fn test_cancel() {
        let sched = Scheduler::new();
        let ev = make_event("bob", "alice", 100, "cancel me");
        let id = ev.id.clone();
        sched.insert(ev);
        sched.insert(make_event("bob", "alice", 200, "keep"));

        let cancelled = sched.cancel_if_ea(&id, 0).ok();
        assert!(cancelled.is_some());
        assert_eq!(cancelled.unwrap().payload, "cancel me");
        assert_eq!(sched.list_by_ea(0).len(), 1);
    }

    #[test]
    fn test_cancel_nonexistent() {
        let sched = Scheduler::new();
        sched.insert(make_event("bob", "alice", 100, "keep"));
        assert!(sched.cancel_if_ea("no-such-id", 0).is_err());
        assert_eq!(sched.list_by_ea(0).len(), 1);
    }

    #[test]
    fn test_list_by_receiver() {
        let sched = Scheduler::new();
        sched.insert(make_event("bob", "alice", 100, "for bob"));
        sched.insert(make_event("carol", "alice", 200, "for carol"));
        sched.insert(make_event("bob", "dave", 300, "also for bob"));

        let bob_events: Vec<_> = sched
            .list_by_ea(0)
            .into_iter()
            .filter(|e| e.receiver == "bob")
            .collect();
        assert_eq!(bob_events.len(), 2);
        assert!(bob_events.iter().all(|e| e.receiver == "bob"));

        let carol_events: Vec<_> = sched
            .list_by_ea(0)
            .into_iter()
            .filter(|e| e.receiver == "carol")
            .collect();
        assert_eq!(carol_events.len(), 1);
    }

    #[test]
    fn test_list_by_ea() {
        let sched = Scheduler::new();
        let mut ev1 = make_event("bob", "alice", 100, "ea0");
        ev1.ea_id = 0;
        let mut ev2 = make_event("carol", "alice", 200, "ea1");
        ev2.ea_id = 1;
        let mut ev3 = make_event("dave", "alice", 300, "ea0 too");
        ev3.ea_id = 0;
        sched.insert(ev1);
        sched.insert(ev2);
        sched.insert(ev3);

        assert_eq!(sched.list_by_ea(0).len(), 2);
        assert_eq!(sched.list_by_ea(1).len(), 1);
        assert_eq!(sched.list_by_ea(99).len(), 0);
    }

    #[test]
    fn test_cancel_by_ea() {
        let sched = Scheduler::new();
        let mut ev1 = make_event("bob", "alice", 100, "ea0");
        ev1.ea_id = 0;
        let mut ev2 = make_event("carol", "alice", 200, "ea1");
        ev2.ea_id = 1;
        sched.insert(ev1);
        sched.insert(ev2);

        let count = sched.cancel_by_ea(0);
        assert_eq!(count, 1);
        assert_eq!(sched.list_by_ea(1).len(), 1);
        assert_eq!(sched.list_by_ea(1)[0].ea_id, 1);
    }

    #[test]
    fn test_pop_batch() {
        let sched = Scheduler::new();
        sched.insert(make_event("bob", "alice", 100, "a"));
        sched.insert(make_event("bob", "carol", 100, "b"));
        sched.insert(make_event("bob", "dave", 200, "c"));
        sched.insert(make_event("carol", "alice", 100, "d"));

        let batch = sched.pop_batch("bob", 0, 100);
        assert_eq!(batch.len(), 2);
        assert!(batch
            .iter()
            .all(|e| e.receiver == "bob" && e.timestamp == 100));

        // Remaining: bob@200 and carol@100
        assert_eq!(sched.list_by_ea(0).len(), 2);
    }

    #[test]
    fn test_pop_batch_ea_scoped() {
        // Fix V7: pop_batch must scope by ea_id to prevent cross-EA batching
        let sched = Scheduler::new();
        let mut ev0 = make_event("auth", "alice", 100, "ea0-event");
        ev0.ea_id = 0;
        let mut ev1 = make_event("auth", "bob", 100, "ea1-event");
        ev1.ea_id = 1;
        sched.insert(ev0);
        sched.insert(ev1);

        // Pop only EA 0's auth events at timestamp 100
        let batch = sched.pop_batch("auth", 0, 100);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].ea_id, 0);
        assert_eq!(batch[0].payload, "ea0-event");

        // EA 1's auth event should still be in the queue
        let remaining = sched.list_by_ea(1);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].ea_id, 1);
        assert_eq!(remaining[0].payload, "ea1-event");
    }

    #[test]
    fn test_cancel_by_receiver() {
        let sched = Scheduler::new();
        sched.insert(make_event("bob", "alice", 100, "a"));
        sched.insert(make_event("bob", "carol", 200, "b"));
        sched.insert(make_event("carol", "alice", 300, "c"));
        sched.insert(make_event("bob", "dave", 400, "d"));

        let cancelled = sched.cancel_by_receiver_and_ea("bob", 0);
        assert_eq!(cancelled, 3);
        let remaining = sched.list_by_ea(0);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].receiver, "carol");
    }

    #[test]
    fn test_cancel_by_receiver_none() {
        let sched = Scheduler::new();
        sched.insert(make_event("bob", "alice", 100, "a"));
        let cancelled = sched.cancel_by_receiver_and_ea("nobody", 0);
        assert_eq!(cancelled, 0);
        assert_eq!(sched.list_by_ea(0).len(), 1);
    }

    #[test]
    fn test_cancel_by_receiver_and_ea() {
        let sched = Scheduler::new();
        // EA 0 has "auth" events
        let mut ev1 = make_event("auth", "alice", 100, "ea0-auth-1");
        ev1.ea_id = 0;
        let mut ev2 = make_event("auth", "carol", 200, "ea0-auth-2");
        ev2.ea_id = 0;
        // EA 1 also has "auth" events
        let mut ev3 = make_event("auth", "dave", 300, "ea1-auth-1");
        ev3.ea_id = 1;
        // EA 0 has "bob" events
        let mut ev4 = make_event("bob", "alice", 400, "ea0-bob");
        ev4.ea_id = 0;
        sched.insert(ev1);
        sched.insert(ev2);
        sched.insert(ev3);
        sched.insert(ev4);

        // Cancel only EA 0's "auth" events — EA 1's "auth" and EA 0's "bob" survive
        let cancelled = sched.cancel_by_receiver_and_ea("auth", 0);
        assert_eq!(cancelled, 2);
        let remaining_ea0 = sched.list_by_ea(0);
        let remaining_ea1 = sched.list_by_ea(1);
        assert_eq!(remaining_ea0.len() + remaining_ea1.len(), 2);
        // Verify EA 1's auth event survived
        assert!(remaining_ea1.iter().any(|e| e.receiver == "auth"));
        // Verify EA 0's bob event survived
        assert!(remaining_ea0.iter().any(|e| e.receiver == "bob"));
    }

    #[test]
    fn test_cancel_if_ea_correct() {
        let sched = Scheduler::new();
        let mut ev = make_event("auth", "alice", 100, "ea0-event");
        ev.ea_id = 0;
        let id = ev.id.clone();
        sched.insert(ev);

        // Cancel with correct EA — should succeed
        let result = sched.cancel_if_ea(&id, 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().payload, "ea0-event");
        assert_eq!(sched.list_by_ea(0).len(), 0);
    }

    #[test]
    fn test_cancel_if_ea_wrong() {
        let sched = Scheduler::new();
        let mut ev = make_event("auth", "alice", 100, "ea0-event");
        ev.ea_id = 0;
        let id = ev.id.clone();
        sched.insert(ev);

        // Cancel with wrong EA — should fail and leave event in queue
        let result = sched.cancel_if_ea(&id, 1);
        assert!(result.is_err());
        assert!(result.unwrap_err()); // true = wrong EA (event exists but wrong owner)
                                      // Event must still be in the queue (atomic — no TOCTOU window)
        let remaining = sched.list_by_ea(0);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].payload, "ea0-event");
    }

    #[test]
    fn test_cancel_if_ea_not_found() {
        let sched = Scheduler::new();
        sched.insert(make_event("bob", "alice", 100, "keep"));

        // Cancel nonexistent event
        let result = sched.cancel_if_ea("no-such-id", 0);
        assert!(result.is_err());
        assert!(!result.unwrap_err()); // false = not found at all
        assert_eq!(sched.list_by_ea(0).len(), 1);
    }

    #[test]
    fn test_format_single_event() {
        let ev = make_event("bob", "alice", 1000, "hello world");
        let msg = format_delivery(&[ev], 1000);
        assert!(msg.contains("[EVENT at t=1000]"));
        assert!(msg.contains("From alice: hello world"));
        assert!(!msg.contains("BATCH"));
    }

    #[test]
    fn test_format_batch() {
        let e1 = make_event("bob", "alice", 1000, "msg1");
        let e2 = make_event("bob", "carol", 1000, "msg2");
        let msg = format_delivery(&[e1, e2], 1000);
        assert!(msg.contains("[EVENT BATCH at t=1000]"));
        assert!(msg.contains("[EVENT] From alice: msg1"));
        assert!(msg.contains("[EVENT] From carol: msg2"));
    }

    #[test]
    fn test_format_cron_event() {
        let mut ev = make_event("bob", "alice", 1000, "periodic check");
        ev.recurring_ns = Some(60_000_000_000);
        let msg = format_delivery(&[ev], 1000);
        assert!(msg.contains("[CRON at t=1000]"));
        assert!(msg.contains("From alice: periodic check"));
        assert!(!msg.contains("[EVENT"));
    }

    #[test]
    fn test_format_batch_mixed() {
        let e1 = make_event("bob", "alice", 1000, "one-time");
        let mut e2 = make_event("bob", "carol", 1000, "recurring");
        e2.recurring_ns = Some(30_000_000_000);
        let msg = format_delivery(&[e1, e2], 1000);
        assert!(msg.contains("[EVENT] From alice: one-time"));
        assert!(msg.contains("[CRON] From carol: recurring"));
    }

    #[test]
    fn test_split_batch_for_quota_defers_excess_events() {
        let mut e1 = make_event("bob", "alice", 1000, "first");
        e1.created_at = 1;
        let mut e2 = make_event("bob", "alice", 1000, "second");
        e2.created_at = 2;
        let mut e3 = make_event("bob", "alice", 1000, "third");
        e3.created_at = 3;

        let (deliver_now, deferred) = split_batch_for_quota(vec![e3, e1, e2], 2);

        assert_eq!(deliver_now.len(), 2);
        assert_eq!(deferred.len(), 1);
        assert_eq!(deliver_now[0].payload, "first");
        assert_eq!(deliver_now[1].payload, "second");
        assert_eq!(deferred[0].payload, "third");
    }

    // ── TickerBuffer tests ──

    #[test]
    fn test_ticker_push_and_render() {
        let ticker = TickerBuffer::new();
        ticker.push("hello");
        ticker.push("world");
        let out = ticker.render(std::time::Duration::from_secs(30));
        assert_eq!(out, "hello +++ world");
    }

    #[test]
    fn test_ticker_empty_render() {
        let ticker = TickerBuffer::new();
        let out = ticker.render(std::time::Duration::from_secs(30));
        assert_eq!(out, "");
    }

    #[test]
    fn test_ticker_expiry() {
        let ticker = TickerBuffer::new();
        ticker.push("old");
        // Render with zero TTL — everything expires immediately
        let out = ticker.render(std::time::Duration::ZERO);
        assert_eq!(out, "");
    }

    #[test]
    fn test_ticker_capacity_cap() {
        let ticker = TickerBuffer::new();
        for i in 0..60 {
            ticker.push(format!("msg{}", i));
        }
        let buf = ticker.entries.lock().unwrap();
        assert_eq!(buf.len(), 50);
        // Oldest entries should have been dropped
        assert_eq!(buf.front().unwrap().text, "msg10");
    }

    // ── input preservation threshold — pure heuristic ──

    // ── opencode TUI extraction helpers ──

    // ── Popup-defer decision (pure helper) ──
    //
    // When the user opens the tmux popup for an agent pane, the scheduler
    // must defer events bound for that pane so injected [EVENT]/[CRON]/
    // [SLACK MESSAGE] lines don't corrupt input the user is typing.
    //
    // `should_defer_for_popup` makes the decision based only on the popup
    // state; the surrounding loop then picks the defer window.

    #[test]
    fn popup_decision_defer_when_receiver_and_ea_match() {
        let popup = new_popup_receiver();
        *popup.lock().unwrap() = Some(("worker".to_string(), 7));
        assert!(should_defer_for_popup(&popup, "worker", 7));
    }

    #[test]
    fn popup_decision_no_defer_when_receiver_differs() {
        let popup = new_popup_receiver();
        *popup.lock().unwrap() = Some(("worker".to_string(), 7));
        assert!(!should_defer_for_popup(&popup, "other", 7));
    }

    #[test]
    fn popup_decision_no_defer_when_ea_differs() {
        // Fix V5 style per-EA scoping: same-named worker in a different EA
        // must NOT be deferred.
        let popup = new_popup_receiver();
        *popup.lock().unwrap() = Some(("worker".to_string(), 7));
        assert!(!should_defer_for_popup(&popup, "worker", 8));
    }

    #[test]
    fn popup_decision_no_defer_when_no_popup_open() {
        let popup = new_popup_receiver();
        assert!(!should_defer_for_popup(&popup, "worker", 7));
    }

    #[test]
    fn popup_decision_defer_ea_alias_matches_omar() {
        // Popup opened on the EA pane (stored as "ea") must defer events
        // addressed either as "ea" or as "omar" — both route to the manager
        // session in `deliver_to_tmux`.
        let popup = new_popup_receiver();
        *popup.lock().unwrap() = Some(("ea".to_string(), 3));
        assert!(should_defer_for_popup(&popup, "ea", 3));
        assert!(should_defer_for_popup(&popup, "omar", 3));
        // Sanity: worker in the same EA still delivers normally
        assert!(!should_defer_for_popup(&popup, "worker", 3));
    }

    #[test]
    fn popup_decision_ea_alias_still_scoped_per_ea() {
        let popup = new_popup_receiver();
        *popup.lock().unwrap() = Some(("ea".to_string(), 3));
        // Events for the EA in a different EA namespace are NOT deferred.
        assert!(!should_defer_for_popup(&popup, "ea", 4));
        assert!(!should_defer_for_popup(&popup, "omar", 4));
    }

    #[test]
    fn popup_decision_alias_is_one_way() {
        // A worker legitimately named "omar" (an unfortunate but legal short
        // name — the receiver field is free-form) must not hijack the EA
        // defer path. Only a popup stored as the canonical "ea" covers the
        // "omar" alias; a popup on worker "omar" covers only "omar".
        let popup = new_popup_receiver();
        *popup.lock().unwrap() = Some(("omar".to_string(), 2));
        assert!(should_defer_for_popup(&popup, "omar", 2));
        assert!(!should_defer_for_popup(&popup, "ea", 2));
    }

    #[test]
    fn event_loop_defers_entire_batch_as_a_batch() {
        // Multiple events sharing the same (receiver, ea_id, timestamp) are
        // deferred together — the deferred batch stays one batch, not fanned
        // out or deduplicated.
        use tokio::runtime::Runtime;
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let scheduler = Arc::new(Scheduler::new());
            let ticker = TickerBuffer::new();
            let popup_receiver = new_popup_receiver();
            *popup_receiver.lock().unwrap() = Some(("popup-target".to_string(), 5));

            let due_ts = now_ns().saturating_sub(500_000_000);
            for i in 0..3 {
                let mut ev = make_event("popup-target", "sender", due_ts, &format!("batch-{}", i));
                ev.ea_id = 5;
                scheduler.insert(ev);
            }

            let loop_handle = tokio::spawn(run_event_loop(
                scheduler.clone(),
                ticker.clone(),
                popup_receiver.clone(),
                "omar-agent-".to_string(),
            ));
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            loop_handle.abort();

            let events = scheduler.list_by_ea(5);
            assert_eq!(events.len(), 3, "all 3 events must remain queued");
            // All should share a freshly-deferred timestamp (~30s from now).
            for ev in &events {
                let delta_secs = (ev.timestamp as i128 - now_ns() as i128) / 1_000_000_000;
                assert!(
                    (25..=31).contains(&delta_secs),
                    "each event must be deferred ~30s; delta={}s",
                    delta_secs
                );
            }
        });
    }

    // ── Event-loop behaviour with popup state ──
    //
    // Regression check: with the popup open for `(receiver, ea_id)`, a
    // past-due event for the same target must be re-queued ~30s into the
    // future instead of being delivered. Without the popup it would be
    // popped and handed to `deliver_to_tmux`.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn event_loop_defers_event_while_popup_open() {
        let scheduler = Arc::new(Scheduler::new());
        let ticker = TickerBuffer::new();
        let popup_receiver = new_popup_receiver();
        *popup_receiver.lock().unwrap() = Some(("popup-target".to_string(), 42));

        // Past-due event — loop will try to deliver immediately
        let mut ev = make_event(
            "popup-target",
            "sender",
            now_ns().saturating_sub(1_000_000_000),
            "hi",
        );
        ev.ea_id = 42;
        scheduler.insert(ev);

        let loop_handle = tokio::spawn(run_event_loop(
            scheduler.clone(),
            ticker.clone(),
            popup_receiver.clone(),
            "omar-agent-".to_string(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        loop_handle.abort();

        let events = scheduler.list_by_ea(42);
        assert_eq!(
            events.len(),
            1,
            "event must stay queued while popup is open"
        );
        let delta_secs = (events[0].timestamp as i128 - now_ns() as i128) / 1_000_000_000;
        assert!(
            (25..=31).contains(&delta_secs),
            "event must be deferred ~30s into future; delta={}s",
            delta_secs
        );

        let ticker_text = ticker.render(std::time::Duration::from_secs(60));
        assert!(
            ticker_text.contains("deferred event(s) for popup-target"),
            "ticker must note the defer: {}",
            ticker_text
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn event_loop_keeps_deferring_while_popup_stays_open() {
        // The popup can stay open longer than 30s. Each delivery attempt
        // must re-defer another 30s — no eventual delivery, no unbounded
        // queue growth.
        let scheduler = Arc::new(Scheduler::new());
        let ticker = TickerBuffer::new();
        let popup_receiver = new_popup_receiver();
        *popup_receiver.lock().unwrap() = Some(("popup-target".to_string(), 1));

        let mut ev = make_event(
            "popup-target",
            "sender",
            now_ns().saturating_sub(500_000_000),
            "hi",
        );
        ev.ea_id = 1;
        scheduler.insert(ev);

        let loop_handle = tokio::spawn(run_event_loop(
            scheduler.clone(),
            ticker.clone(),
            popup_receiver.clone(),
            "omar-agent-".to_string(),
        ));

        // Run the loop for long enough to see multiple defer decisions.
        // We can't wait 30s in a unit test, so instead artificially pull
        // the event back into "due" territory and observe that the loop
        // re-defers each time.
        for _ in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let mut q = scheduler.queue.lock().unwrap();
            let mut events: Vec<ScheduledEvent> = q.drain().collect();
            for e in events.iter_mut() {
                e.timestamp = now_ns().saturating_sub(10_000_000);
            }
            for e in events {
                q.push(e);
            }
            drop(q);
            scheduler.notify.notify_one();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        loop_handle.abort();

        // Queue still holds exactly one event (no duplicates, no delivery).
        let events = scheduler.list_by_ea(1);
        assert_eq!(events.len(), 1, "event must not be lost or duplicated");
        let delta_secs = (events[0].timestamp as i128 - now_ns() as i128) / 1_000_000_000;
        assert!(
            delta_secs > 25,
            "event must still be deferred; delta={}s",
            delta_secs
        );
    }

    #[test]
    fn test_persistent_scheduler_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = events_store_path(dir.path());
        let sched = Scheduler::with_store(store_path.clone());

        let mut ev = make_event("alice", "manager", 100, "persist me");
        ev.ea_id = 7;
        sched.insert(ev);

        let reloaded = Scheduler::with_store(store_path);
        let events = reloaded.list_by_ea(7);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, "persist me");
        assert_eq!(events[0].receiver, "alice");
    }

    #[cfg(unix)]
    #[test]
    fn persistent_scheduler_reclaims_stale_store_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = events_store_path(dir.path());
        let lock_path = store_lock_path(&store_path);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, u32::MAX.to_string()).unwrap();

        let sched = Scheduler::with_store(store_path);
        let mut ev = make_event("alice", "manager", 100, "persist after stale lock");
        ev.ea_id = 7;
        sched.insert(ev);

        assert!(
            !lock_path.exists(),
            "stale scheduler lock should be reclaimed and cleaned up"
        );
        assert_eq!(sched.list_by_ea(7).len(), 1);
    }
}
