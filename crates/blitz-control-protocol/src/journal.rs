use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use nagoya::sync::Notify;

use crate::{DebugEvent, DiagnosticRecord, TargetContext};

#[derive(Clone, Debug, PartialEq)]
pub enum JournalItem {
    Event(Arc<DiagnosticRecord>),
    Overflow { first: u64, last: u64 },
}

impl JournalItem {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::Event(event) => event.sequence,
            Self::Overflow { last, .. } => *last,
        }
    }
}

#[derive(Default)]
struct State {
    sequence: u64,
    records: VecDeque<Arc<DiagnosticRecord>>,
}

pub struct DiagnosticJournal {
    capacity: usize,
    state: Mutex<State>,
    changed: Notify,
}

impl DiagnosticJournal {
    pub fn new(capacity: usize) -> Arc<Self> {
        assert!(capacity > 0);
        Arc::new(Self {
            capacity,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        })
    }

    pub fn publish(&self, context: TargetContext, event: DebugEvent) {
        let mut state = self.state.lock().unwrap();
        state.sequence = state
            .sequence
            .checked_add(1)
            .expect("diagnostic sequence exhausted");
        let sequence = state.sequence;
        if state.records.len() == self.capacity {
            state.records.pop_front();
        }
        state.records.push_back(Arc::new(DiagnosticRecord {
            context,
            sequence,
            event,
        }));
        drop(state);
        self.changed.notify_waiters();
    }

    pub fn sequence(&self) -> u64 {
        self.state.lock().unwrap().sequence
    }

    pub fn read_after(&self, seen: u64) -> Option<JournalItem> {
        let state = self.state.lock().unwrap();
        let first = state.records.front()?.sequence;
        if seen < first - 1 {
            return Some(JournalItem::Overflow {
                first: seen + 1,
                last: first - 1,
            });
        }
        state
            .records
            .iter()
            .find(|record| record.sequence > seen)
            .cloned()
            .map(JournalItem::Event)
    }

    pub async fn next_after(&self, seen: u64) -> JournalItem {
        loop {
            let notified = self.changed.notified();
            futures::pin_mut!(notified);
            let ready = notified.as_mut().enable();
            if let Some(item) = self.read_after(seen) {
                return item;
            }
            if !ready {
                notified.await;
            }
        }
    }
}
