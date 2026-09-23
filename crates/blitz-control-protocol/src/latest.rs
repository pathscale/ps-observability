//! Latest-value handoffs, without a channel.
//!
//! Two shapes this crate needs and nagoya deliberately does not ship, written
//! against what it does: an `RwLock` for the value and a `Notify` for the wake.
//!
//! Neither is a queue. A `watch` retains one revision and a `oneshot` carries
//! one answer, so a channel was always more machinery than the job wanted; what
//! was being used was the retained slot and the wake.
//!
//! Both follow the order WorkTable's `RuntimeNotified` documents: register the
//! waiter with `enable` *before* reading the slot. `notify_waiters` stores no
//! permit, so a waiter that reads first can lose a transition that lands
//! between the read and the first poll.

use std::sync::Arc;

use nagoya::sync::{Notify, RwLock};

/// The newest value, and a wake when it changes.
///
/// Replaces a `watch`. Writers overwrite, readers observe the newest, and a
/// reader that falls behind skips the revisions it missed rather than queueing
/// them. That is the property the event stream wanted: inspection stays bounded
/// when a client stalls, or when an animation presents faster than the client
/// consumes.
#[derive(Debug)]
pub struct Latest<T> {
    slot: RwLock<Option<T>>,
    changed: Notify,
}

impl<T: Clone> Latest<T> {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: RwLock::new(None),
            changed: Notify::new(),
        })
    }

    /// Replace the value and wake everyone waiting.
    ///
    /// The store happens before the wake, so an observer that sees the wake and
    /// then reads finds the new value.
    pub async fn set(&self, value: T) {
        *self.slot.write().await = Some(value);
        self.changed.notify_waiters();
    }

    /// The current value, if any.
    pub async fn get(&self) -> Option<T> {
        self.slot.read().await.clone()
    }

    /// Wait for the next change, then return the value.
    pub async fn changed(&self) -> Option<T> {
        let notified = self.changed.notified();
        futures::pin_mut!(notified);
        // Registered before the read below, and `enable` reports a wake that
        // already landed so it is not waited for twice.
        if !notified.as_mut().enable() {
            notified.await;
        }
        self.get().await
    }
}

/// One value, delivered once.
///
/// Replaces a `oneshot`. The sender fills the slot and wakes; the receiver
/// takes it. A sender that goes away without sending calls `abandon`, so a
/// caller waiting on an answer that will never come observes `None` rather than
/// parking forever. That is the distinction a dropped `oneshot` sender draws,
/// and it is the one that matters here: a control request whose runtime went
/// away has to fail, not hang the client.
#[derive(Debug)]
pub struct Once<T> {
    /// The value and the finality flag under one lock.
    ///
    /// Two locks would not be readable together: a receiver could see an empty
    /// slot, lose the race to a `send`, then read a `done` that is now true and
    /// report `None` while the value sits in the slot. One lock makes "is there
    /// a value, and is this final" a single question.
    state: RwLock<(Option<T>, bool)>,
    ready: Notify,
}

impl<T> Once<T> {
    /// An unfilled slot.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new((None, false)),
            ready: Notify::new(),
        })
    }

    /// Fill the slot and wake the receiver. A second send is ignored.
    pub async fn send(&self, value: T) {
        {
            let mut state = self.state.write().await;
            if state.0.is_none() {
                state.0 = Some(value);
            }
            state.1 = true;
        }
        self.ready.notify_waiters();
    }

    /// Wake the receiver without filling the slot.
    ///
    /// What a sender does when it is going away: the receiver takes `None` and
    /// stops waiting, which is the difference between a cancelled request and a
    /// hang.
    pub async fn abandon(&self) {
        self.state.write().await.1 = true;
        self.ready.notify_waiters();
    }

    /// Take the value, waiting until it arrives or the sender abandons it.
    ///
    /// `enable` registers this waiter before the slot is read, which is the
    /// order `Notified::enable` documents: `notify_waiters` stores no permit, so
    /// a waiter that read first could lose a transition landing between the read
    /// and the first poll.
    ///
    /// The loop is not belt and braces. `notify_waiters` wakes every waiter, so
    /// a wake observed here need not be the one meant for this caller; the
    /// finality flag is what makes an answer final, and anything else goes back
    /// to waiting rather than reporting a false `None`.
    pub async fn recv(&self) -> Option<T> {
        loop {
            let notified = self.ready.notified();
            futures::pin_mut!(notified);
            let ready = notified.as_mut().enable();

            {
                let mut state = self.state.write().await;
                if let Some(value) = state.0.take() {
                    return Some(value);
                }
                if state.1 {
                    return None;
                }
            }

            if !ready {
                notified.await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race `enable` exists for: a value that lands between a reader's
    /// decision to wait and its first poll must still wake it.
    #[test]
    fn a_value_sent_before_the_first_poll_is_not_lost() {
        let once = Once::<u8>::new();
        let writer = Arc::clone(&once);
        nagoya::block_on(async move {
            writer.send(7).await;
            assert_eq!(once.recv().await, Some(7));
        });
    }

    /// A sender that goes away answers, rather than leaving the caller parked.
    #[test]
    fn an_abandoned_slot_answers_none() {
        let once = Once::<u8>::new();
        let writer = Arc::clone(&once);
        nagoya::block_on(async move {
            writer.abandon().await;
            assert_eq!(once.recv().await, None);
        });
    }

    /// Taking the value is what makes it gone: a second take finds nothing,
    /// and finality stops it waiting for a value that will never come.
    #[test]
    fn a_value_is_delivered_once() {
        let once = Once::<u8>::new();
        let writer = Arc::clone(&once);
        nagoya::block_on(async move {
            writer.send(1).await;
            assert_eq!(once.recv().await, Some(1));
            assert_eq!(once.recv().await, None);
        });
    }

    /// Latest keeps the newest revision rather than queueing: a reader that
    /// was not looking sees the last write, not the first.
    #[test]
    fn latest_retains_only_the_newest() {
        let latest = Latest::<u8>::new();
        nagoya::block_on(async move {
            latest.set(1).await;
            latest.set(2).await;
            latest.set(3).await;
            assert_eq!(latest.get().await, Some(3));
        });
    }
}
