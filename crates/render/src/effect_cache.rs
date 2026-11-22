//! Bounded, exact-input memoization for expensive pure pixel effects.
//!
//! Sampling only rejects obvious misses. A hit always compares every input float's bits,
//! including alpha, before restoring the output. No source id, timestamp or hash alone can
//! authorize reuse. A busy cache is bypassed: a Rayon worker must never wait on another
//! worker that may need its pool to finish an effect.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{TryLockError, TryLockResult};

use crate::Image;

const MAX_ENTRIES: usize = 4;

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub entries: Option<usize>,
    pub bytes: Option<usize>,
    pub limit_bytes: usize,
    pub busy: bool,
}

struct Entry<K> {
    key: K,
    width: usize,
    height: usize,
    sample: u64,
    input: Vec<f32>,
    output: Vec<f32>,
}

struct State<K> {
    entries: VecDeque<Entry<K>>,
    bytes: usize,
}

pub(crate) struct EffectCache<K> {
    state: Mutex<State<K>>,
    limit: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    bypasses: AtomicU64,
}

fn recover<T>(lock: TryLockResult<T>) -> Option<T> {
    match lock {
        Ok(value) => Some(value),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn copy(values: &[f32], reusable: Option<Vec<f32>>) -> Option<Vec<f32>> {
    if let Some(mut output) = reusable {
        output.copy_from_slice(values);
        return Some(output);
    }
    let mut output = Vec::new();
    output.try_reserve_exact(values.len()).ok()?;
    output.extend_from_slice(values);
    Some(output)
}

fn sample(values: &[f32]) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    values.len().hash(&mut hash);
    for value in values.iter().step_by((values.len() / 64).max(1)).take(64).chain(values.last()) {
        value.to_bits().hash(&mut hash);
    }
    hash.finish()
}

impl<K: Eq> EffectCache<K> {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State { entries: VecDeque::new(), bytes: 0 }),
            limit,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            bypasses: AtomicU64::new(0),
        }
    }

    pub(crate) fn stats(&self) -> EffectCacheStats {
        let state = recover(self.state.try_lock());
        EffectCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            bypasses: self.bypasses.load(Ordering::Relaxed),
            entries: state.as_ref().map(|s| s.entries.len()),
            bytes: state.as_ref().map(|s| s.bytes),
            limit_bytes: self.limit,
            busy: state.is_none(),
        }
    }

    /// `compute` returns false for failed or cancelled work: it must not enter the cache.
    pub(crate) fn apply(&self, image: &mut Image, key: K, compute: impl FnOnce(&mut Image) -> bool) {
        let required = image.px.len().checked_mul(size_of::<f32>()).and_then(|bytes| bytes.checked_mul(2));
        let valid = image.w.checked_mul(image.h).and_then(|n| n.checked_mul(4)) == Some(image.px.len());
        let Some(required) = required.filter(|n| valid && *n > 0 && *n <= self.limit) else {
            self.bypasses.fetch_add(1, Ordering::Relaxed);
            compute(image);
            return;
        };
        let Some(mut state) = recover(self.state.try_lock()) else {
            self.bypasses.fetch_add(1, Ordering::Relaxed);
            compute(image);
            return;
        };
        let fingerprint = sample(&image.px);
        let found = state.entries.iter().position(|entry| {
            entry.key == key
                && entry.width == image.w
                && entry.height == image.h
                && entry.sample == fingerprint
                && entry.input.len() == image.px.len()
                && entry.input.iter().zip(&image.px).all(|(a, b)| a.to_bits() == b.to_bits())
        });
        if let Some(entry) = found.and_then(|i| state.entries.remove(i)) {
            image.px.copy_from_slice(&entry.output);
            state.entries.push_back(entry);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        // Reserve the full input AND output before allocating either. Keeping the try-lock
        // across compute also bounds in-flight cache copies; contending workers bypass it.
        let mut reusable = None;
        while state.bytes > self.limit.saturating_sub(required) || state.entries.len() >= MAX_ENTRIES {
            let Some(old) = state.entries.pop_front() else { break };
            state.bytes = state.bytes.saturating_sub(old.input.len().saturating_mul(size_of::<f32>()).saturating_mul(2));
            // Reuse only equally sized buffers: retaining larger capacities would hide
            // memory outside the budget. This avoids allocation/page-fault churn when
            // successive full-size frames are unique.
            if old.input.len() == image.px.len() {
                reusable = Some((old.input, old.output));
            }
        }
        let (input_buffer, output_buffer) = reusable.map_or((None, None), |(input, output)| (Some(input), Some(output)));
        let Some(input) = copy(&image.px, input_buffer) else {
            drop(state);
            compute(image);
            return;
        };
        let (width, height) = (image.w, image.h);
        if !compute(image) || image.w != width || image.h != height || image.px.len() != input.len() {
            return;
        }
        let Some(output) = copy(&image.px, output_buffer) else { return };
        if state.entries.try_reserve(1).is_err() {
            return;
        }
        state.entries.push_back(Entry { key, width, height, sample: fingerprint, input, output });
        state.bytes = state.bytes.saturating_add(required);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Image {
        Image::filled(16, 8, [0.2, 0.4, 0.6, 1.0])
    }

    fn computed(image: &mut Image) -> bool {
        for pixel in &mut image.px {
            *pixel *= 0.5;
        }
        true
    }

    #[test]
    fn repeated_pixels_reuse_output_and_key_changes_invalidate() {
        let cache = EffectCache::new(16 * 8 * 4 * 4 * 4);
        let original = image();
        let mut first = original.clone();
        cache.apply(&mut first, 1, computed);
        let mut repeated = original.clone();
        cache.apply(&mut repeated, 1, |_| panic!("repeated pixels must not recompute"));
        assert_eq!(first, repeated);
        let mut changed = original.clone();
        cache.apply(&mut changed, 2, computed);
        assert_eq!((cache.stats().hits, cache.stats().misses), (1, 2));
        assert!(cache.stats().bytes.unwrap() <= cache.stats().limit_bytes);
    }

    #[test]
    fn sampled_collision_and_alpha_change_cannot_reuse_a_frame() {
        let cache = EffectCache::new(1 << 20);
        let mut a = Image::filled(256, 1, [0.2, 0.4, 0.6, 1.0]);
        let mut b = a.clone();
        b.px[3] = 0.5;
        assert_eq!(sample(&a.px), sample(&b.px), "change an unsampled alpha value");
        cache.apply(&mut a, 1, computed);
        cache.apply(&mut b, 1, computed);
        assert_ne!(a, b);
        assert_eq!((cache.stats().hits, cache.stats().misses), (0, 2));
    }

    #[test]
    fn failed_work_and_geometry_changes_are_never_cached() {
        let cache = EffectCache::new(1 << 20);
        cache.apply(&mut image(), 1, |_| false);
        cache.apply(&mut image(), 1, |image| {
            image.w = 1;
            true
        });
        assert_eq!(cache.stats().entries, Some(0));
    }

    #[test]
    fn budget_entry_limit_and_oversized_bypass_are_bounded() {
        let bytes = image().px.len() * size_of::<f32>() * 2;
        let cache = EffectCache::new(bytes);
        for key in 0..20 {
            cache.apply(&mut image(), key, computed);
        }
        assert_eq!(cache.stats().entries, Some(1));
        assert_eq!(cache.stats().bytes, Some(bytes));
        let small = EffectCache::new(bytes - 1);
        small.apply(&mut image(), 1, computed);
        assert_eq!((small.stats().entries, small.stats().bypasses), (Some(0), 1));
        let many = EffectCache::new(1 << 20);
        for key in 0..20 {
            many.apply(&mut Image::filled(1, 1, [1.0; 4]), key, computed);
        }
        assert_eq!(many.stats().entries, Some(MAX_ENTRIES));
    }

    #[test]
    fn a_busy_cache_bypasses_instead_of_waiting_for_its_own_rayon_pool() {
        let cache = EffectCache::new(1 << 20);
        cache.apply(&mut image(), 1, |image| {
            rayon::scope(|scope| {
                scope.spawn(|_| cache.apply(&mut super::tests::image(), 1, computed));
            });
            computed(image)
        });
        assert_eq!(cache.stats().bypasses, 1);
        assert_eq!(cache.stats().entries, Some(1));
    }

    #[test]
    fn least_recently_used_entries_leave_first() {
        let cache = EffectCache::new(image().px.len() * size_of::<f32>() * 4);
        for key in [0, 1, 0, 2, 0, 1] {
            cache.apply(&mut image(), key, computed);
        }
        assert_eq!((cache.stats().hits, cache.stats().misses), (2, 4));
        assert_eq!(cache.stats().entries, Some(2));
    }

    #[test]
    fn unique_frames_recycle_evicted_storage_without_stale_pixels() {
        let bytes = image().px.len() * size_of::<f32>() * 2;
        let cache = EffectCache::new(bytes);
        cache.apply(&mut image(), 0, computed);
        let buffers = {
            let state = cache.state.lock().unwrap();
            let entry = state.entries.front().unwrap();
            (entry.input.as_ptr(), entry.output.as_ptr())
        };
        for key in 1..20 {
            let mut next = image();
            next.px[3] = key as f32;
            let mut expected = next.clone();
            computed(&mut expected);
            cache.apply(&mut next, key, computed);
            assert_eq!(next, expected);
            let state = cache.state.lock().unwrap();
            let entry = state.entries.front().unwrap();
            assert_eq!((entry.input.as_ptr(), entry.output.as_ptr()), buffers);
            assert_eq!(state.bytes, bytes);
        }
        assert_eq!(cache.stats().misses, 20);
    }

    #[test]
    fn an_interrupted_compute_does_not_poison_later_requests() {
        let cache = EffectCache::new(1 << 20);
        assert!(std::panic::catch_unwind(|| cache.apply(&mut image(), 1, |_| panic!("synthetic failed worker"))).is_err());
        let mut actual = image();
        cache.apply(&mut actual, 1, computed);
        let mut expected = image();
        computed(&mut expected);
        assert_eq!(actual, expected);
        assert_eq!(cache.stats().entries, Some(1));
    }
}
