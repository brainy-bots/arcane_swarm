//! Per-frame decode cache shared by all Arcane drain tasks.
//!
//! The cluster broadcasts the same delta bytes to every connected player
//! ("full mesh" replication today). On the swarm-as-driver side, that means
//! `decode_server` on the same bytes runs once per player per broadcast —
//! identical work, multiplied by N. At 5750 players × 76 frames/sec each
//! that's 437K full-payload decodes per second, ~700 cores' worth of work
//! on c7i.2xlarge. Single-driver capacity in this regime is tens of players,
//! not thousands — see swarm scaling notes.
//!
//! The cache flips it: each unique broadcast is decoded **once**, and every
//! player consults the cached entity-id set to compute its own latency. The
//! total decode work becomes O(M broadcasts/sec) regardless of player count
//! (where M = clusters × tick_rate). That's ~76 decodes/sec for the current
//! topology, a >5000× reduction at scale.
//!
//! ## Honesty
//!
//! This is *not* cheating. The cluster's behavior is unchanged — it still
//! produces, encodes, and sends one broadcast per (cluster, tick) per
//! connected client. Each client socket on the driver still receives, TCP-
//! acks, and consumes its bytes. Per-player latency timestamps T1 (send) and
//! T_arrival (drain wakeup) are still captured per-player at their actual
//! socket events. Only the *decode* step — which on a real client wouldn't
//! be a per-broadcast O(N) scan over all entities anyway — is shared across
//! the simulated clients in the same driver process. That's a fair use of
//! "all these simulated players are running in one process and receiving
//! broadcasts whose bytes are bit-for-bit identical".
//!
//! In a partial-mesh / AOI world the cache hit rate would drop (different
//! players get different bytes), but the data structure stays correct.
//! Today's full-mesh wire makes it maximally effective.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// One decoded broadcast's contribution to latency measurement.
///
/// We don't keep the whole `DeltaPayload` — we only need the entity-id set
/// (so each player can ask "is my id in this delta?") and the server-side
/// wall-clock timestamp the cluster stamped on it (T2 in the latency
/// decomposition). Holding a small set keeps cache memory bounded — at
/// 1438 UUIDs × 16 bytes ≈ 23 KB per entry, the cap below comfortably
/// covers seconds of broadcasts across all clusters.
pub struct CachedDelta {
    pub entity_ids: HashSet<Uuid>,
    pub client_seqs: HashMap<Uuid, u64>,
}

/// Bounded hash-keyed cache of decoded broadcast deltas.
///
/// The key is a SipHash of the full frame. FlatBuffer field layout puts
/// tick/seq/timestamp past byte 32, so a prefix key would collide across
/// ticks from the same cluster. Full-frame hashing is still orders of
/// magnitude cheaper than a FlatBuffer decode.
///
/// Eviction is simple FIFO. Recent broadcasts dominate lookups so a young
/// entry rarely gets evicted before its lookups settle.
pub struct DeltaCache {
    inner: Mutex<DeltaCacheInner>,
    max_entries: usize,
}

struct DeltaCacheInner {
    map: HashMap<u64, Arc<CachedDelta>>,
    order: VecDeque<u64>,
    hits: u64,
    misses: u64,
}

impl DeltaCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(DeltaCacheInner {
                map: HashMap::new(),
                order: VecDeque::new(),
                hits: 0,
                misses: 0,
            }),
            max_entries,
        }
    }

    fn key_for(bytes: &[u8]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        hasher.finish()
    }

    /// Look up the cached decode for a frame. Returns `Some` on hit, `None`
    /// on miss. Counts hits/misses for the hit-rate diagnostic in the
    /// reporter output.
    pub fn lookup(&self, bytes: &[u8]) -> Option<Arc<CachedDelta>> {
        let key = Self::key_for(bytes);
        let mut g = self.inner.lock().ok()?;
        // Clone the Arc out of the immutable borrow before we touch the hit
        // counter, so the borrow checker sees them as non-overlapping.
        let entry = g.map.get(&key).cloned();
        match entry {
            Some(e) => {
                g.hits += 1;
                Some(e)
            }
            None => {
                g.misses += 1;
                None
            }
        }
    }

    /// Insert a fresh decode result. If multiple drain tasks raced and each
    /// decoded redundantly before the first insert landed, the duplicate
    /// inserts are harmless — they overwrite with equivalent data.
    pub fn insert(&self, bytes: &[u8], entry: Arc<CachedDelta>) {
        let key = Self::key_for(bytes);
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        // Eviction: drop the oldest insertion if at capacity. Simple FIFO
        // (not strict LRU). Since drain tasks consult the cache within a
        // few hundred ms of insert, FIFO is functionally close to LRU.
        while g.map.len() >= self.max_entries {
            if let Some(old) = g.order.pop_front() {
                g.map.remove(&old);
            } else {
                break;
            }
        }
        if !g.map.contains_key(&key) {
            g.order.push_back(key);
        }
        g.map.insert(key, entry);
    }

    /// Snapshot the hit/miss counters and reset them. Wired into the
    /// FINAL output so we can see how often the cache actually saved a
    /// decode at each tier.
    pub fn snapshot_and_reset_counters(&self) -> (u64, u64) {
        match self.inner.lock() {
            Ok(mut g) => {
                let h = g.hits;
                let m = g.misses;
                g.hits = 0;
                g.misses = 0;
                (h, m)
            }
            Err(_) => (0, 0),
        }
    }
}

impl Default for DeltaCache {
    fn default() -> Self {
        // Sized for ~5s of broadcasts at 4 clusters × 20 Hz = 400 entries,
        // with headroom. Memory bound: 1000 × 23 KB ≈ 23 MB worst case.
        Self::new(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_frames_get_distinct_keys() {
        let mut frame_a = vec![0u8; 80];
        let mut frame_b = vec![0u8; 80];
        // Identical prefix (simulates FlatBuffer vtable/offset region),
        // differ only in later bytes (simulates seq/tick/timestamp fields).
        frame_a[40] = 1;
        frame_b[40] = 2;
        assert_ne!(
            DeltaCache::key_for(&frame_a),
            DeltaCache::key_for(&frame_b),
            "frames differing only past byte 32 must produce different cache keys"
        );
    }

    #[test]
    fn lookup_returns_inserted_entry() {
        let cache = DeltaCache::new(10);
        let frame = b"test-frame-bytes";
        let entry = Arc::new(CachedDelta {
            entity_ids: HashSet::new(),
            client_seqs: HashMap::new(),
        });
        assert!(cache.lookup(frame).is_none());
        cache.insert(frame, entry.clone());
        assert!(cache.lookup(frame).is_some());
    }

    #[test]
    fn eviction_respects_capacity() {
        let cache = DeltaCache::new(2);
        for i in 0u8..3 {
            let frame = vec![i; 16];
            cache.insert(
                &frame,
                Arc::new(CachedDelta {
                    entity_ids: HashSet::new(),
                    client_seqs: HashMap::new(),
                }),
            );
        }
        // First entry should have been evicted.
        assert!(cache.lookup(&vec![0u8; 16]).is_none());
        assert!(cache.lookup(&vec![1u8; 16]).is_some());
        assert!(cache.lookup(&vec![2u8; 16]).is_some());
    }

    #[test]
    fn hit_miss_counters_track_correctly() {
        let cache = DeltaCache::new(10);
        let frame = b"counter-test";
        cache.insert(
            frame,
            Arc::new(CachedDelta {
                entity_ids: HashSet::new(),
                client_seqs: HashMap::new(),
            }),
        );
        cache.lookup(frame); // hit
        cache.lookup(frame); // hit
        cache.lookup(b"missing"); // miss
        let (hits, misses) = cache.snapshot_and_reset_counters();
        assert_eq!(hits, 2);
        assert_eq!(misses, 1);
        let (h2, m2) = cache.snapshot_and_reset_counters();
        assert_eq!(h2, 0);
        assert_eq!(m2, 0);
    }
}
