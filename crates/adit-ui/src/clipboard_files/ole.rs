//! The STA thread that owns `OleSetClipboard`.
//!
//! ## Why a thread of its own
//!
//! `OleSetClipboard` is apartment-bound: it must run on a thread that has called
//! `OleInitialize` (which enters an STA) and that keeps pumping messages, because
//! OLE marshals through a hidden window and the shell calls back into the data
//! object from there. Adit's UI thread is owned by `iced`, cannot be
//! `OleInitialize`d without changing its apartment out from under the renderer,
//! and must not be blocked in any case — CLAUDE.md records what that looks like.
//!
//! FreeRDP reaches the same conclusion and does the same thing: a dedicated
//! clipboard thread, woken by a posted message, owning every OLE call.
//!
//! ## What crosses the boundary
//!
//! The offer, not the object. Building the `IDataObject` here rather than on the
//! caller's thread means no COM pointer is ever passed between apartments, which
//! would need real marshalling to be correct. Everything in [`Offer`] is plain
//! `Send` data.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use adit_session::ClipFile;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{OleInitialize, OleSetClipboard, OleUninitialize};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG,
    PM_NOREMOVE, WM_APP,
};

use super::bridge::ChunkBridge;
use super::data_object::{ChunkRequester, FileDataObject};

/// Wake-up message: "there is an offer waiting on the channel".
const WM_OFFER: u32 = WM_APP + 1;

/// Everything needed to publish one file list, all of it `Send`.
pub(crate) struct Offer {
    pub(crate) files: Vec<ClipFile>,
    pub(crate) bridge: ChunkBridge,
    pub(crate) request: ChunkRequester,
    pub(crate) chunk: u32,
}

struct OleThread {
    offers: Mutex<Sender<Offer>>,
    thread_id: u32,
}

static OLE_THREAD: OnceLock<Option<OleThread>> = OnceLock::new();

/// Publish a file list on the Windows clipboard.
///
/// Returns without waiting: the actual `OleSetClipboard` happens on the OLE
/// thread. Nothing is read from the remote here — the object is delay-rendered,
/// so the wire stays quiet until something pastes.
pub(crate) fn offer_to_clipboard(offer: Offer) {
    let Some(thread) = OLE_THREAD.get_or_init(start_ole_thread) else {
        return; // the thread could not start; the local half is simply absent
    };
    let queued = thread
        .offers
        .lock()
        .map(|sender| sender.send(offer).is_ok())
        .unwrap_or(false);
    if !queued {
        return;
    }
    // SAFETY: posting to a thread id that existed when the thread published it.
    // A failed post means the thread is gone, in which case the offer sits on
    // the channel harmlessly.
    unsafe {
        let _ = PostThreadMessageW(thread.thread_id, WM_OFFER, WPARAM(0), LPARAM(0));
    }
}

fn start_ole_thread() -> Option<OleThread> {
    let (offer_tx, offer_rx) = channel::<Offer>();
    // The thread id is only knowable from inside the thread, so it comes back
    // out before anything is queued to it.
    let (id_tx, id_rx) = channel::<u32>();

    std::thread::Builder::new()
        .name("adit-ole-clipboard".into())
        .spawn(move || {
            // SAFETY: OleInitialize enters an STA for this thread and is paired
            // with OleUninitialize on every path out. Nothing else on this thread
            // touches COM before it.
            unsafe {
                if OleInitialize(None).is_err() {
                    return;
                }
            }

            // PostThreadMessage silently fails against a thread that has no
            // message queue, and the queue is only created once the thread has
            // asked for a message. So force one into being before publishing the
            // id — otherwise the very first offer is dropped, which reads as
            // "the first copy after launch never works".
            let mut message = MSG::default();
            unsafe {
                let _ = PeekMessageW(&mut message, None, WM_OFFER, WM_OFFER, PM_NOREMOVE);
            }
            if id_tx.send(unsafe { GetCurrentThreadId() }).is_err() {
                unsafe { OleUninitialize() };
                return;
            }

            // SAFETY: a standard message loop. `GetMessageW` returns 0 on WM_QUIT
            // and -1 on error; both end it.
            while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                if message.message == WM_OFFER {
                    // Drain: several offers can be queued behind one post, and
                    // only the newest describes what the clipboard now holds.
                    if let Some(offer) = offer_rx.try_iter().last() {
                        publish(offer);
                    }
                    continue;
                }
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            unsafe { OleUninitialize() };
        })
        .ok()?;

    let thread_id = id_rx.recv().ok()?;
    Some(OleThread {
        offers: Mutex::new(offer_tx),
        thread_id,
    })
}

/// Hand one data object to the clipboard. Runs on the OLE thread only.
fn publish(offer: Offer) {
    let object: IDataObject =
        FileDataObject::new(offer.files, offer.bridge, offer.request, offer.chunk).into();

    // SAFETY: on the OLE-initialised thread, with a live data object. Ownership
    // passes to OLE, which releases it when the clipboard next changes hands.
    //
    // Deliberately no OleFlushClipboard: flushing renders every format eagerly,
    // which for this object means downloading every file the remote copied. The
    // entire point is that nothing crosses until someone pastes. The cost is
    // that the offer dies with Adit — which is what mstsc does too.
    unsafe {
        if let Err(error) = OleSetClipboard(Some(&object)) {
            // Nearly always another process holding the clipboard. Not worth
            // retrying: the remote's next copy re-offers, and a retry loop on
            // this thread would delay that.
            let _ = error;
        }
    }
}
