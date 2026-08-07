//! The blocking bridge between Explorer's paste and the RDP helper.
//!
//! ## The problem this exists to solve
//!
//! A local paste of remote files runs inside `IStream::Read`, which Explorer
//! calls **synchronously** and expects to return bytes. The bytes are three
//! hops away — through the helper process, over the RDP wire, and back — and
//! that round trip is asynchronous. So something has to block.
//!
//! FreeRDP solves this with a Win32 event per request (`req_fevent`), signalled
//! by its response handler. This is the same shape with two differences that
//! matter here:
//!
//! * **Requests are keyed.** FreeRDP has effectively one outstanding request;
//!   keying by stream id means several files can be in flight, which is what
//!   Explorer does when you paste a folder.
//! * **Every wait has a deadline.** A server that never answers, or a helper
//!   that died mid-transfer, would otherwise park an Explorer thread forever —
//!   and an Explorer thread stuck in a COM call takes the window with it.
//!
//! ## What must not happen
//!
//! The waiting happens on *Explorer's* thread, never on the UI thread. Adit has
//! already been bitten by blocking the UI thread — it surfaces as "Not
//! Responding" with no other symptom (see CLAUDE.md), and a paste of a large
//! file would freeze the whole app for the length of the transfer. Nothing here
//! may be called from `update`.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long one chunk may take before the read is failed.
///
/// Generous, because the round trip crosses a possibly slow link and the remote
/// may be reading from spinning disk — but finite, because the alternative is a
/// wedged Explorer window. FreeRDP waits indefinitely here; that is the one part
/// of its design this deliberately does not copy.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on requests in flight at once. Explorer opens a stream per file, and
/// a paste of a large folder would otherwise let an unbounded map grow.
const MAX_IN_FLIGHT: usize = 256;

/// One request's slot: `None` while in flight, `Some` once answered.
type Slot = Option<Result<Vec<u8>, ChunkError>>;

/// Why a chunk did not arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkError {
    /// The remote refused, or the helper could not read it.
    Refused,
    /// Nothing arrived within [`CHUNK_TIMEOUT`].
    TimedOut,
    /// The session ended while this read was waiting.
    Disconnected,
    /// Too many reads already in flight.
    TooManyRequests,
}

#[derive(Debug, Default)]
struct Inner {
    slots: HashMap<u32, Slot>,
    /// Set when the session ends, so waiters stop rather than sit out their
    /// full timeout one after another — a folder paste against a dead session
    /// would otherwise take `files × 30s` to give up.
    closed: bool,
    next_stream_id: u32,
}

/// Shared between the COM streams (which wait) and the app's event pump (which
/// answers). Cloneable; every clone refers to the same set of slots.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChunkBridge {
    inner: Arc<(Mutex<Inner>, Condvar)>,
}

impl ChunkBridge {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reserve a stream id and register it as in flight.
    ///
    /// Taken before the request is sent, not after, so a response that arrives
    /// before the caller starts waiting still lands in a slot that exists. The
    /// opposite order loses the race on a fast link, which is the kind of bug
    /// that only shows up against a LAN host.
    pub(crate) fn begin(&self) -> Result<u32, ChunkError> {
        let (lock, _) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return Err(ChunkError::Disconnected);
        }
        if inner.slots.len() >= MAX_IN_FLIGHT {
            return Err(ChunkError::TooManyRequests);
        }
        // Wrapping is fine and collisions are not a concern in practice: an id
        // is retired as soon as its wait ends, so 2^32 would have to be issued
        // while one is still outstanding.
        inner.next_stream_id = inner.next_stream_id.wrapping_add(1);
        let id = inner.next_stream_id;
        inner.slots.insert(id, None);
        Ok(id)
    }

    /// Block until the chunk for `stream_id` arrives, or the deadline passes.
    ///
    /// **Never call this from the UI thread.** It is meant for the COM stream's
    /// `Read`, which runs on Explorer's thread.
    pub(crate) fn wait(&self, stream_id: u32) -> Result<Vec<u8>, ChunkError> {
        let (lock, condvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + CHUNK_TIMEOUT;

        loop {
            match inner.slots.get(&stream_id) {
                // Answered: take the slot away with the result.
                Some(Some(_)) => {
                    return inner
                        .slots
                        .remove(&stream_id)
                        .flatten()
                        .unwrap_or(Err(ChunkError::Refused));
                }
                // Still in flight.
                Some(None) => {}
                // Never registered, or already collected. Either way there is
                // nothing coming.
                None => return Err(ChunkError::Refused),
            }
            if inner.closed {
                inner.slots.remove(&stream_id);
                return Err(ChunkError::Disconnected);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                inner.slots.remove(&stream_id);
                return Err(ChunkError::TimedOut);
            };
            let (guard, _) = condvar
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
        }
    }

    /// Deliver a chunk. `None` marks the read as refused.
    ///
    /// Answering an id nobody is waiting for is not an error — a timed-out wait
    /// removes its own slot, and the late response then arrives here with
    /// nowhere to go. Dropping it is correct; the alternative is a slot that is
    /// never collected.
    pub(crate) fn deliver(&self, stream_id: u32, data: Option<Vec<u8>>) {
        let (lock, condvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = inner.slots.get_mut(&stream_id) {
            *slot = Some(data.ok_or(ChunkError::Refused));
            condvar.notify_all();
        }
    }

    /// Fail every waiter and every future one. Called when the session ends, so
    /// a paste in progress stops immediately instead of timing out file by file.
    pub(crate) fn close(&self) {
        let (lock, condvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.closed = true;
        condvar.notify_all();
    }

    /// How many reads are registered. Test-only: it is a racy number in
    /// production and only meaningful when nothing else is running.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordinary path: a reader blocks, the pump answers, the bytes come out
    /// verbatim — binary included, since that is the whole point.
    #[test]
    fn a_waiting_read_receives_the_bytes_it_was_sent() {
        let bridge = ChunkBridge::new();
        let id = bridge.begin().expect("a stream id");

        let answering = bridge.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            answering.deliver(id, Some(vec![0u8, 0xFF, 0x00, 0x7F]));
        });

        assert_eq!(bridge.wait(id).expect("bytes"), [0, 0xFF, 0, 0x7F]);
        handle.join().expect("answering thread");
        // The slot is collected, not leaked.
        assert_eq!(bridge.in_flight(), 0);
    }

    /// A response that beats the reader to the lock must not be lost. `begin`
    /// registers the slot before the request goes out precisely so this works;
    /// on a LAN the answer really can arrive first.
    #[test]
    fn a_response_that_arrives_before_the_wait_is_still_collected() {
        let bridge = ChunkBridge::new();
        let id = bridge.begin().expect("a stream id");

        bridge.deliver(id, Some(vec![1, 2, 3]));

        assert_eq!(bridge.wait(id).expect("bytes"), [1, 2, 3]);
    }

    /// A refusal is a distinct outcome from empty bytes. Returning `Ok(vec![])`
    /// here would tell Explorer "end of file" and write a truncated file.
    #[test]
    fn a_refusal_is_not_an_empty_chunk() {
        let bridge = ChunkBridge::new();

        let refused = bridge.begin().expect("id");
        bridge.deliver(refused, None);
        assert_eq!(bridge.wait(refused), Err(ChunkError::Refused));

        let empty = bridge.begin().expect("id");
        bridge.deliver(empty, Some(Vec::new()));
        assert_eq!(bridge.wait(empty), Ok(Vec::new()));
    }

    /// Closing the session releases waiters at once rather than leaving each to
    /// sit out its own timeout — a folder paste would otherwise take
    /// files × CHUNK_TIMEOUT to give up, with Explorer wedged throughout.
    #[test]
    fn closing_releases_a_waiter_immediately() {
        let bridge = ChunkBridge::new();
        let id = bridge.begin().expect("a stream id");

        let closing = bridge.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            closing.close();
        });

        let started = Instant::now();
        assert_eq!(bridge.wait(id), Err(ChunkError::Disconnected));
        assert!(
            started.elapsed() < CHUNK_TIMEOUT,
            "close must not wait out the timeout"
        );
        handle.join().expect("closing thread");

        // And nothing new can be started afterwards.
        assert_eq!(bridge.begin(), Err(ChunkError::Disconnected));
    }

    /// An id nobody registered, or one already collected, fails rather than
    /// blocking for the full timeout on a slot that will never be filled.
    #[test]
    fn waiting_on_an_unknown_stream_fails_at_once() {
        let bridge = ChunkBridge::new();
        let started = Instant::now();
        assert_eq!(bridge.wait(4242), Err(ChunkError::Refused));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Delivering to a stream that timed out and cleaned itself up is a no-op,
    /// not a panic and not a resurrected slot.
    #[test]
    fn a_late_response_to_a_collected_stream_is_dropped() {
        let bridge = ChunkBridge::new();
        let id = bridge.begin().expect("a stream id");
        bridge.deliver(id, Some(vec![9]));
        assert_eq!(bridge.wait(id).expect("bytes"), [9]);

        // The slot is gone; this must not recreate it.
        bridge.deliver(id, Some(vec![9]));
        assert_eq!(bridge.in_flight(), 0);
    }

    /// The in-flight cap holds, so a runaway paste cannot grow the map without
    /// bound.
    #[test]
    fn the_in_flight_cap_is_enforced() {
        let bridge = ChunkBridge::new();
        for _ in 0..MAX_IN_FLIGHT {
            bridge.begin().expect("under the cap");
        }
        assert_eq!(bridge.begin(), Err(ChunkError::TooManyRequests));
    }
}
