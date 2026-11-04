//! `ImageCache` and `PreparedImages` from `server/images.py`: a byte-budgeted LRU of prepared images keyed by the
//! raw bytes and pixel budget, so images resent in chat history are decoded once, plus a separate budget for the
//! pixels of requests in flight.

use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{prepare, ImageError, PreparedImage};

/// Cached prepared images.
pub const BUDGET_BYTES: usize = 256 * 1024 * 1024;
/// Pixels of requests in flight.
pub const REQUEST_BUDGET_BYTES: usize = 256 * 1024 * 1024;

type Key = [u8; 32];

struct State {
    entries: HashMap<Key, (Arc<PreparedImage>, u64)>,
    /// Use tick to key, least recently used first.
    order: BTreeMap<u64, Key>,
    tick: u64,
    bytes: usize,
    request_bytes: usize,
}

struct Inner {
    state: Mutex<State>,
    budget_bytes: usize,
    request_budget_bytes: usize,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Shared by clone.
#[derive(Clone)]
pub struct ImageCache {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageCacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub budget_bytes: usize,
    pub request_bytes: usize,
    pub request_budget_bytes: usize,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::with_budgets(BUDGET_BYTES, REQUEST_BUDGET_BYTES)
    }
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_budgets(budget_bytes: usize, request_budget_bytes: usize) -> Self {
        let state = State { entries: HashMap::new(), order: BTreeMap::new(), tick: 0, bytes: 0, request_bytes: 0 };
        Self { inner: Arc::new(Inner { state: Mutex::new(state), budget_bytes, request_budget_bytes }) }
    }

    /// An empty batch charged against this cache's in-flight budget.
    pub fn request_batch(&self) -> PreparedImages {
        PreparedImages { cache: self.inner.clone(), images: Vec::new(), bytes: 0 }
    }

    /// The cached image for these bytes and budget, or a fresh one, prepared outside the lock and then kept.
    pub fn prepare(&self, payload: &[u8], max_pixels: u32) -> Result<Arc<PreparedImage>, ImageError> {
        let mut hasher = Sha256::new();
        hasher.update(max_pixels.to_le_bytes());
        hasher.update(payload);
        let key: Key = hasher.finalize().into();
        {
            let mut state = self.inner.lock();
            if let Some((image, tick)) = state.entries.get(&key).map(|(i, t)| (i.clone(), *t)) {
                state.touch(key, tick);
                return Ok(image);
            }
        }
        let image = Arc::new(prepare(payload, max_pixels)?);
        self.inner.lock().put(key, image.clone(), self.inner.budget_bytes);
        Ok(image)
    }

    pub fn stats(&self) -> ImageCacheStats {
        let state = self.inner.lock();
        ImageCacheStats {
            entries: state.entries.len(),
            bytes: state.bytes,
            budget_bytes: self.inner.budget_bytes,
            request_bytes: state.request_bytes,
            request_budget_bytes: self.inner.request_budget_bytes,
        }
    }
}

impl State {
    fn touch(&mut self, key: Key, old: u64) {
        self.order.remove(&old);
        self.tick += 1;
        self.order.insert(self.tick, key);
        self.entries.get_mut(&key).unwrap().1 = self.tick;
    }

    /// Python's `LRUCache.put`: a value larger than the whole budget is not kept.
    fn put(&mut self, key: Key, image: Arc<PreparedImage>, budget: usize) {
        let size = image.pixels.len();
        if size > budget {
            return;
        }
        if let Some((old, tick)) = self.entries.remove(&key) {
            self.order.remove(&tick);
            self.bytes -= old.pixels.len();
        }
        self.tick += 1;
        self.order.insert(self.tick, key);
        self.entries.insert(key, (image, self.tick));
        self.bytes += size;
        while self.bytes > budget {
            let (_, evicted) = self.order.pop_first().expect("bytes imply entries");
            let (old, _) = self.entries.remove(&evicted).unwrap();
            self.bytes -= old.pixels.len();
        }
    }
}

/// One request's images, in render order. Each occurrence is charged, even a cache hit, because concatenation and
/// the native transport repeat those pixels; the charge returns when the last holder drops the batch, so keep it
/// alive for as long as the engine or a mask task still works on the request.
pub struct PreparedImages {
    cache: Arc<Inner>,
    images: Vec<Arc<PreparedImage>>,
    bytes: usize,
}

impl PreparedImages {
    pub fn push(&mut self, image: Arc<PreparedImage>) -> Result<(), ImageError> {
        let size = image.pixels.len();
        let mut state = self.cache.lock();
        if size > self.cache.request_budget_bytes - state.request_bytes {
            return Err(ImageError::Capacity("in-flight image memory budget is full".into()));
        }
        state.request_bytes += size;
        self.bytes += size;
        drop(state);
        self.images.push(image);
        Ok(())
    }
}

impl Deref for PreparedImages {
    type Target = [Arc<PreparedImage>];
    fn deref(&self) -> &Self::Target {
        &self.images
    }
}

impl Drop for PreparedImages {
    fn drop(&mut self) {
        self.cache.lock().request_bytes -= self.bytes;
    }
}
