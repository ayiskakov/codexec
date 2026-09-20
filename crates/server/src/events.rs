//! In-process fan-out of submission state changes to SSE subscribers.
//! The cluster version of this is the result event bus (NATS in the design doc).

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::store::Submission;

#[derive(Default)]
pub struct Hub {
    channels: Mutex<HashMap<String, broadcast::Sender<Submission>>>,
}

impl Hub {
    pub fn subscribe(&self, id: &str) -> broadcast::Receiver<Submission> {
        let mut channels = self.channels.lock().unwrap();
        channels.entry(id.to_string()).or_insert_with(|| broadcast::channel(64).0).subscribe()
    }

    /// Delivers to current subscribers; a no-op when nobody listens.
    pub fn publish(&self, sub: &Submission) {
        let channels = self.channels.lock().unwrap();
        if let Some(tx) = channels.get(&sub.id) {
            let _ = tx.send(sub.clone());
        }
    }

    /// Drops channels without subscribers. Called by the sweeper.
    pub fn prune(&self) -> usize {
        let mut channels = self.channels.lock().unwrap();
        let before = channels.len();
        channels.retain(|_, tx| tx.receiver_count() > 0);
        before - channels.len()
    }
}
