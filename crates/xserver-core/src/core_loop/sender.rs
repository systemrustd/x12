//! `CoreSender` / `CoreReceiver`: an `mpsc`-style channel that wakes
//! the core's mio poller on every send.
//!
//! Producers (reader threads, libinput, signalfd watcher, setup
//! threads) hold a `CoreSender`; the core owns the `CoreReceiver` and
//! the `Poll`. `NOTIFY_TOKEN` is the token the receiver registers for
//! channel readiness — when a poll iteration sees it, drain the
//! receiver via `try_recv_all`.

use std::{io, sync::Arc};

use crossbeam_channel::{Receiver, Sender};
use mio::{Poll, Token, Waker};

use super::message::Message;

pub const NOTIFY_TOKEN: Token = Token(0);

#[derive(Clone)]
pub struct CoreSender {
    waker: Arc<Waker>,
    tx: Sender<Message>,
}

pub struct CoreReceiver {
    rx: Receiver<Message>,
}

/// Build the (poll, sender, receiver) triple. The waker is registered
/// against `NOTIFY_TOKEN`; producers calling `CoreSender::send` will
/// cause the next `poll.poll()` to surface that token.
pub fn channel() -> io::Result<(Poll, CoreSender, CoreReceiver)> {
    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), NOTIFY_TOKEN)?);
    let (tx, rx) = crossbeam_channel::unbounded();
    Ok((poll, CoreSender { waker, tx }, CoreReceiver { rx }))
}

impl CoreSender {
    pub fn send(&self, m: Message) -> io::Result<()> {
        self.tx
            .send(m)
            .map_err(|_| io::Error::other("core receiver dropped"))?;
        self.waker.wake()
    }

    /// Cheap clone for handing to producer threads.
    #[must_use]
    pub fn clone_handle(&self) -> Self {
        self.clone()
    }
}

impl CoreReceiver {
    /// Drain everything currently buffered. Non-blocking; stops at the
    /// first empty `try_recv`.
    pub fn try_recv_all(&self) -> impl Iterator<Item = Message> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sender_wakes_poll() {
        let (mut poll, sender, _rx) = channel().unwrap();
        sender.clone_handle().send(Message::Shutdown).unwrap();
        let mut events = mio::Events::with_capacity(4);
        poll.poll(&mut events, Some(Duration::from_millis(50)))
            .unwrap();
        assert!(events.iter().any(|e| e.token() == NOTIFY_TOKEN));
    }
}
