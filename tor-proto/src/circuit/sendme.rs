//! Management for flow control windows.
//!
//! Tor maintains a separate windows on circuits and on streams.
//! These are controlled by SENDME cells, which (confusingly) are
//! applied either at the circuit or the steam level depending on
//! whether they have a stream ID set.
//!
//! Circuit sendmes are _authenticated_: they include a cryptographic
//! tag generated by the cryptography layer.  This tag proves that the
//! other side of the circuit really has read all of the data that it's
//! acknowledging.

use futures::channel::oneshot;
use futures::lock::Mutex;

use std::collections::VecDeque;
use std::sync::Arc;

use tor_cell::relaycell::msg::RelayMsg;
use tor_cell::relaycell::RelayCell;

// XXXX Three problems with this tag:
// XXXX - First, we need to support unauthenticated flow control.
// XXXX - Second, this tag type could be different for each layer, if we
// XXXX   eventually have an authenticator that isn't 20 bytes long.
// XXXX - Third, we want the comparison to happen with a constant-time
// XXXX   operation.

/// Tag type used in regular v1 sendme cells.
pub type CircTag = [u8; 20];
/// Absence of a tag, as with stream cells.
pub type NoTag = ();

/// A circuit's send window.
pub type CircSendWindow = SendWindow<CircParams, CircTag>;
/// A stream's send window.
pub type StreamSendWindow = SendWindow<StreamParams, NoTag>;

/// A circuit's receive window.
pub type CircRecvWindow = RecvWindow<CircParams>;
/// A stream's receive window.
pub type StreamRecvWindow = RecvWindow<StreamParams>;

/// Tracks how many cells we can safely send on a circuit or stream.
///
/// Additionally, remembers a list of tags that could be used to
/// acknowledge the cells we have already sent, so we know it's safe
/// to send more.
pub struct SendWindow<P, T>
where
    P: WindowParams,
    T: PartialEq + Eq + Clone,
{
    // TODO could use a bilock if that becomes non-experimental.
    // TODO I wish we could do this without locking; we could make a bunch
    // of these functions non-async if that happened.
    /// Actual SendWindow object.
    w: Arc<Mutex<SendWindowInner<T>>>,
    /// Marker type to tell the compiler that the P type is used.
    _dummy: std::marker::PhantomData<P>,
}

/// Interior (locked) code for SendWindowInner.
struct SendWindowInner<T>
where
    T: PartialEq + Eq + Clone,
{
    /// Current value for this window
    window: u16,
    /// Tag values that incoming "SENDME" messages need to match in order
    /// for us to send more data.
    tags: VecDeque<T>,
    /// If present, a oneshot that we are blocking on before we can send
    /// any more data.
    unblock: Option<oneshot::Sender<()>>,
}

/// Helper: parameterizes a window to determine its maximum and its increment.
pub trait WindowParams {
    /// Largest allowable value for this window.
    fn maximum() -> u16;
    /// Increment for this window.
    fn increment() -> u16;
}

/// Parameters used for SENDME windows on circuits: limit at 1000 cells,
/// and each SENDME adjusts by 100.
#[derive(Clone)]
pub struct CircParams;
impl WindowParams for CircParams {
    fn maximum() -> u16 {
        1000
    }
    fn increment() -> u16 {
        100
    }
}

/// Parameters used for SENDME windows on streams: limit at 500 cells,
/// and each SENDME adjusts by 50.
#[derive(Clone)]
pub struct StreamParams;
impl WindowParams for StreamParams {
    fn maximum() -> u16 {
        500
    }
    fn increment() -> u16 {
        50
    }
}

impl<P, T> SendWindow<P, T>
where
    P: WindowParams,
    T: PartialEq + Eq + Clone,
{
    /// Construct a new SendWindow.
    pub fn new(window: u16) -> SendWindow<P, T> {
        let increment = P::increment();
        let capacity = (window + increment - 1) / increment;
        let inner = SendWindowInner {
            window,
            tags: VecDeque::with_capacity(capacity as usize),
            unblock: None,
        };
        SendWindow {
            w: Arc::new(Mutex::new(inner)),
            _dummy: std::marker::PhantomData,
        }
    }

    /// Add a reference-count to SendWindow and return a new handle to it.
    pub fn new_ref(&self) -> Self {
        SendWindow {
            w: Arc::clone(&self.w),
            _dummy: std::marker::PhantomData,
        }
    }

    /// Remove one item from this window (since we've sent a cell).
    ///
    /// The provided tag is the one associated with the crypto layer that
    /// originated the cell.  It will get cloned and recorded if we'll
    /// need to check for it later.
    ///
    /// Return the number of cells left in the window
    pub async fn take(&mut self, tag: &T) -> u16 {
        loop {
            let wait_on = {
                let mut w = self.w.lock().await;
                if let Some(val) = w.window.checked_sub(1) {
                    w.window = val;
                    if w.window % P::increment() == 0 {
                        // We record this tag.
                        // TODO: I'm not saying that this cell in particular
                        // matches the spec, but Tor seems to like it.
                        w.tags.push_back(tag.clone());
                    }

                    return val;
                }

                // Window is zero; can't send yet.
                let (send, recv) = oneshot::channel::<()>();

                let old = w.unblock.replace(send);
                assert!(old.is_none()); // XXXXM3 can this happen?
                recv
            };
            // Wait on this receiver while _not_ holding the lock.

            // XXXXM3 Danger: can this unwrap fail? I think it can't, since
            // the sender can't be cancelled as long as there's a refcount
            // to it.
            wait_on.await.unwrap()
        }
    }

    /// Handle an incoming sendme with a provided tag.
    ///
    /// If the tag is None, then we don't enforce tag requirements. (We can
    /// remove this option once we no longer support getting SENDME cells
    /// from relays without the FlowCtrl=1 protocol.)
    ///
    /// On success, return the number of cells left in the window.
    ///
    /// On failure, return None: the caller should close the stream
    /// or circuit with a protocol error.
    #[must_use = "didn't check whether SENDME tag was right."]
    pub async fn put(&mut self, tag: Option<T>) -> Option<u16> {
        let mut w = self.w.lock().await;

        match (w.tags.pop_front(), tag) {
            (Some(t), Some(tag)) if t == tag => {} // this is the right tag.
            (Some(_), None) => {}                  // didn't need a tag.
            _ => {
                return None;
            } // Bad tag or unexpected sendme.
        }

        let v = w.window.checked_add(P::increment())?;
        w.window = v;

        if let Some(send) = w.unblock.take() {
            // if we get a failure, nothing cares about this window any more.
            // XXXXM3 is that true?
            let _ignore = send.send(());
        }

        Some(v)
    }
}

/// Structure to track when we need to send SENDME cells for incoming data.
#[derive(Clone)]
pub struct RecvWindow<P: WindowParams> {
    /// Number of cells that we'd be willing to receive on this window
    /// before sending a SENDME.
    window: u16,
    /// Marker type to tell the compiler that the P type is used.
    _dummy: std::marker::PhantomData<P>,
}

impl<P: WindowParams> RecvWindow<P> {
    /// Create a new RecvWindow.
    pub fn new(window: u16) -> RecvWindow<P> {
        RecvWindow {
            window,
            _dummy: std::marker::PhantomData,
        }
    }

    /// Called when we've just sent a cell; return true if we need to send
    /// a sendme, and false otherwise.
    ///
    /// Returns None if we should not have sent the cell, and we just
    /// violated the window.
    #[must_use]
    pub fn take(&mut self) -> Option<bool> {
        let v = self.window.checked_sub(1);
        if let Some(x) = v {
            self.window = x;
            // TODO: same note as in SendWindow.take(). I don't know if
            // this truly matches the spec, but tor accepts it.
            Some(x % P::increment() == 0)
        } else {
            None
        }
    }

    /// Reduce this window by `n`; give an error if this is not possible.
    pub fn decrement_n(&mut self, n: u16) -> crate::Result<()> {
        let v = self.window.checked_sub(n);
        if let Some(x) = v {
            self.window = x;
            Ok(())
        } else {
            Err(crate::Error::CircProto(
                "Received too many cells on a stream".into(),
            ))
        }
    }

    /// Called when we've just send a SENDME.
    pub fn put(&mut self) {
        self.window = self.window.checked_add(P::increment()).unwrap();
    }
}

/// Return true if this message is counted by flow-control windows.
pub(crate) fn msg_counts_towards_windows(msg: &RelayMsg) -> bool {
    matches!(msg, RelayMsg::Data(_))
}

/// Return true if this message is counted by flow-control windows.
pub(crate) fn cell_counts_towards_windows(cell: &RelayCell) -> bool {
    msg_counts_towards_windows(cell.msg())
}
