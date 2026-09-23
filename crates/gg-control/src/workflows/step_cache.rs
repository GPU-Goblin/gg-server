//! The server's step state kept between rounds (ADR-0036).
//!
//! After a round's optimizer step the server keeps each trained shard's new weights and Adam
//! moments in memory, so the next step reads only the tasks' gradients instead of the whole
//! checkpoint and its optimizer state. The moments are written durably behind the round, while the
//! workers compute the next one; the weights, which workers need, are written before the next
//! round opens.
//!
//! An entry is used only for the exact weights it was computed with (their content hash) at the
//! expected optimizer step; anything else is dropped and the step falls back to reading the
//! checkpoint. Memory is bounded (`step_cache_bytes`): the least recently stored entry goes first.

use std::collections::HashMap;
use std::sync::Mutex;

use gg_compute::graph::task::{Named, NamedOptimizerState};
use gg_protocol::messages::ArtifactRef;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::error::ApiError;

/// Optimizer state after a step, with the references of its moments once written durably.
pub struct Moments {
    /// First moments, durable.
    pub m: Option<ArtifactRef>,
    /// Second moments, durable.
    pub v: Option<ArtifactRef>,
    /// The state itself.
    pub opt: NamedOptimizerState,
}

/// A shard's state after a step.
pub struct Entry {
    /// The weights, as stored (durably) and referenced by the run.
    pub weights_ref: ArtifactRef,
    /// Their values.
    pub weights: Named,
    /// Optimizer steps applied to them.
    pub t: u32,
    /// The moments: being written behind the round, handed back once durable.
    pub moments: JoinHandle<Result<Moments, ApiError>>,
    /// Memory held (weights and moments).
    pub bytes: u64,
}

/// Bounded store of [`Entry`] by run and shard.
pub struct StepCache {
    capacity: u64,
    inner: Mutex<Inner>,
    rebuilt: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<(Uuid, String), (u64, Entry)>,
    used: u64,
    clock: u64,
}

impl StepCache {
    /// A cache holding at most `capacity` bytes (0: disabled).
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner::default()),
            rebuilt: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Bytes it may hold.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The entry of a run's shard, taken out, if it holds exactly `weights` after `t` optimizer
    /// steps. A stale entry for that shard is dropped.
    pub fn take(&self, run: Uuid, key: &str, weights: &ArtifactRef, t: u32) -> Option<Entry> {
        let mut inner = self.lock();
        let (_, entry) = inner.entries.remove(&(run, key.to_owned()))?;
        inner.used -= entry.bytes;
        (entry.weights_ref == *weights && entry.t == t).then_some(entry)
    }

    /// Keeps `entry`, evicting the least recently stored entries until it fits. An entry larger
    /// than the whole capacity is handed back.
    pub fn put(&self, run: Uuid, key: &str, entry: Entry) -> Result<(), Entry> {
        if entry.bytes > self.capacity {
            return Err(entry);
        }
        let mut inner = self.lock();
        if let Some((_, old)) = inner.entries.remove(&(run, key.to_owned())) {
            inner.used -= old.bytes;
        }
        while inner.used + entry.bytes > self.capacity {
            let oldest = inner
                .entries
                .iter()
                .min_by_key(|(_, (clock, _))| *clock)
                .map(|(k, _)| k.clone())
                .expect("used > 0 means an entry");
            let (_, evicted) = inner.entries.remove(&oldest).expect("present");
            inner.used -= evicted.bytes;
            tracing::info!(run = %oldest.0, shard = %oldest.1, bytes = evicted.bytes, "step state evicted from memory");
        }
        inner.clock += 1;
        let clock = inner.clock;
        inner.used += entry.bytes;
        inner.entries.insert((run, key.to_owned()), (clock, entry));
        Ok(())
    }

    /// Drops every entry of a run.
    pub fn remove_run(&self, run: Uuid) {
        let mut inner = self.lock();
        let keys: Vec<_> = inner
            .entries
            .keys()
            .filter(|(r, _)| *r == run)
            .cloned()
            .collect();
        for k in keys {
            let (_, e) = inner.entries.remove(&k).expect("present");
            inner.used -= e.bytes;
        }
    }

    /// Drops everything, as a restart does.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.entries.clear();
        inner.used = 0;
    }

    /// Bytes held.
    pub fn used_bytes(&self) -> u64 {
        self.lock().used
    }

    /// Counts a shard whose moments were rebuilt from a round's gradients.
    pub fn note_rebuilt(&self) {
        self.rebuilt
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Shards whose moments were rebuilt since start-up.
    pub fn rebuilt(&self) -> u64 {
        self.rebuilt.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Bytes of named f32 tensors in memory.
pub fn named_bytes(n: &Named) -> u64 {
    n.values().map(|t| t.data.len() as u64 * 4).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gg_core::Sha256Digest;

    fn entry(tag: &[u8], t: u32, bytes: u64) -> Entry {
        Entry {
            weights_ref: ArtifactRef {
                hash: Sha256Digest::of_bytes(tag),
                size: 1,
            },
            weights: Named::new(),
            t,
            moments: tokio::spawn(async {
                Ok(Moments {
                    m: None,
                    v: None,
                    opt: NamedOptimizerState::default(),
                })
            }),
            bytes,
        }
    }

    #[tokio::test]
    async fn exact_weights_and_step_only_and_bounded() {
        let c = StepCache::new(100);
        let run = Uuid::now_v7();
        let w = entry(b"w1", 3, 40).weights_ref;
        assert!(c.put(run, "checkpoint.0", entry(b"w1", 3, 40)).is_ok());
        // Other weights or another step: dropped, not returned.
        assert!(c.take(run, "checkpoint.0", &w, 4).is_none());
        assert_eq!(c.used_bytes(), 0);
        assert!(c.put(run, "checkpoint.0", entry(b"w1", 3, 40)).is_ok());
        let other = entry(b"w2", 3, 1).weights_ref;
        assert!(c.take(run, "checkpoint.0", &other, 3).is_none());
        assert!(c.put(run, "checkpoint.0", entry(b"w1", 3, 40)).is_ok());
        let e = c.take(run, "checkpoint.0", &w, 3).expect("exact match");
        assert_eq!(e.t, 3);
        assert!(e.moments.await.unwrap().is_ok());
        // Bounded: the oldest entries go first; larger than everything is refused.
        assert!(c.put(run, "a", entry(b"a", 1, 40)).is_ok());
        assert!(c.put(run, "b", entry(b"b", 1, 40)).is_ok());
        assert!(c.put(run, "c", entry(b"c", 1, 40)).is_ok());
        assert_eq!(c.used_bytes(), 80);
        assert!(
            c.take(run, "a", &entry(b"a", 1, 0).weights_ref, 1)
                .is_none()
        );
        assert!(c.put(run, "d", entry(b"d", 1, 101)).is_err());
        c.remove_run(run);
        assert_eq!(c.used_bytes(), 0);
    }
}
