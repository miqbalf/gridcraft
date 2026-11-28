//! A native relay link that never blocks the caller: the desktop window opens it from a frame.
//!
//! [`craft_collab::native::NativeHub::connect_as`] is async and waits for the socket handshake,
//! up to ten seconds. The desktop app opens a link at start-up and again at each ticket renewal
//! (through [`craft_collab::Handover`], whose opener is synchronous), on the UI thread. So
//! [`BgHub`] starts the connect on a Tokio runtime and answers at once: it reads as not connected
//! and delivers nothing until the socket is up, then is the native hub. A connect that fails
//! (the relay is down or unreachable) is tried again with a growing wait, with the newest
//! ticket, until it succeeds or the hub is dropped. The protocol is the native hub's, unchanged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_collab::native::NativeHub;
use craft_collab::{ClientMsg, Hub, ServerMsg};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

/// The first wait after a failed connect, and the longest.
const RETRY_FIRST: Duration = Duration::from_millis(250);
const RETRY_MOST: Duration = Duration::from_secs(5);
/// Messages kept for the socket while it connects, at most (as the native hub keeps while down).
const MAX_QUEUED: usize = 256;

enum State {
    Connecting(oneshot::Receiver<NativeHub>),
    Ready(NativeHub),
    Gone,
}

/// A native hub that connects in the background ([module docs](self)).
pub struct BgHub {
    state: State,
    /// The ticket the next connect attempt uses, shared with the connecting task.
    token: Arc<Mutex<String>>,
    queued: Vec<ClientMsg>,
    space: Option<u32>,
    /// Stops the connecting task when the hub is dropped before it connected.
    stop: Option<oneshot::Sender<()>>,
}

impl BgHub {
    /// Starts connecting to `relay` (`http(s)://host/craft`) on `rt` with `ticket`, naming
    /// `schema` and resuming at `applied` (as [`NativeHub::connect_as`]).
    #[must_use]
    pub fn connect(rt: &Handle, relay: &str, ticket: &str, applied: Option<u64>, schema: Option<&str>) -> BgHub {
        let token = Arc::new(Mutex::new(ticket.to_owned()));
        let (tx, rx) = oneshot::channel();
        let (stop, mut stopped) = oneshot::channel::<()>();
        let (relay, schema, shared) = (relay.to_owned(), schema.map(str::to_owned), token.clone());
        rt.spawn(async move {
            let mut wait = RETRY_FIRST;
            // `applied` only on the first attempt: a retry joins at the saved sequence, as the
            // native hub's own reconnects do.
            let mut applied = applied;
            loop {
                let ticket = shared.lock().map(|t| t.clone()).unwrap_or_default();
                let attempt = NativeHub::connect_as(&relay, &ticket, applied.take(), schema.as_deref());
                tokio::select! {
                    _ = &mut stopped => return,
                    hub = attempt => match hub {
                        Ok(hub) => {
                            let _ = tx.send(hub);
                            return;
                        }
                        // The error names the transport only, never the ticket.
                        Err(e) => log::warn!("live: the relay connection failed, retrying: {e}"),
                    },
                }
                tokio::select! {
                    _ = &mut stopped => return,
                    () = tokio::time::sleep(wait) => {}
                }
                wait = (wait * 2).min(RETRY_MOST);
            }
        });
        BgHub { state: State::Connecting(rx), token, queued: Vec::new(), space: None, stop: Some(stop) }
    }

    /// Takes the socket once its connect finished.
    fn settle(&mut self) {
        let State::Connecting(rx) = &mut self.state else { return };
        let hub = match rx.try_recv() {
            Ok(hub) => hub,
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.state = State::Gone;
                return;
            }
        };
        let mut hub = hub;
        if let Some(space) = self.space {
            hub.set_space(space);
        }
        // A ticket renewed while it connected is the one its reconnects use.
        if let Ok(token) = self.token.lock() {
            hub.set_token(token.clone());
        }
        for msg in self.queued.drain(..) {
            let _ = hub.send(msg);
        }
        self.state = State::Ready(hub);
    }

    /// Whether the socket is up (connected or reconnecting on its own).
    #[must_use]
    pub fn opened(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }
}

impl Hub for BgHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.settle();
        match &mut self.state {
            State::Ready(hub) => hub.send(msg),
            State::Connecting(_) => {
                if self.queued.len() < MAX_QUEUED {
                    self.queued.push(msg);
                }
                Ok(())
            }
            State::Gone => Err(craft_collab::Error::Hub("relay connection is gone".to_owned())),
        }
    }

    fn poll(&mut self) -> Vec<ServerMsg> {
        self.settle();
        match &mut self.state {
            State::Ready(hub) => hub.poll(),
            State::Connecting(_) | State::Gone => Vec::new(),
        }
    }

    fn connected(&self) -> bool {
        match &self.state {
            State::Ready(hub) => hub.connected(),
            State::Connecting(_) | State::Gone => false,
        }
    }

    fn set_token(&mut self, token: String) {
        if let Ok(mut t) = self.token.lock() {
            t.clone_from(&token);
        }
        if let State::Ready(hub) = &mut self.state {
            hub.set_token(token);
        }
    }

    fn set_space(&mut self, space: u32) {
        self.space = Some(space);
        if let State::Ready(hub) = &mut self.state {
            hub.set_space(space);
        }
    }

    fn close(&mut self) {
        if let State::Ready(hub) = &mut self.state {
            hub.close();
        }
        self.queued.clear();
        self.stop = None;
        self.state = State::Gone;
    }
}

/// A desktop link to `relay` with `ticket`: a [`BgHub`] that hands over to a new one, opened the
/// same way, whenever the host renews the ticket ([`Hub::set_token`]), so a renewal drops nothing
/// (the web host's [`craft_collab::Handover`] over its own socket, the same protocol).
#[must_use]
pub fn renewing_link(rt: &Handle, relay: &str, ticket: &str, schema: &str) -> craft_collab::Handover<BgHub> {
    let first = BgHub::connect(rt, relay, ticket, None, Some(schema));
    let (rt, relay, schema) = (rt.clone(), relay.to_owned(), schema.to_owned());
    craft_collab::Handover::new(first, move |r: &craft_collab::Reopen<'_>| Ok(BgHub::connect(&rt, &relay, r.ticket, r.applied, Some(&schema))))
}
