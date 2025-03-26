use std::future::Future;
use std::hash::Hash;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use quick_cache::sync::Cache;

// This ideally shouldn't be too big compared to `--max-threads` size, eventually,
// the deduplicated work should be cached into its eventual storage,
// and the subsequent operations should be returned from the eventual cache.
const CACHE_CAPACITY: usize = 128;

// Singleflight provides an async cache that can be filled atomically without doing a duplicated work.
#[derive(Debug)]
pub struct Singleflight<K: Hash + Eq, V> {
    latest: DashMap<K, Instant>,
    cache: Cache<(K, Instant), V>,
    ttl: Duration,
}

impl<K: Hash + Eq + Clone, V: Clone> Singleflight<K, V> {
    pub fn new(ttl: Duration) -> Singleflight<K, V> {
        Singleflight {
            latest: DashMap::new(),
            cache: Cache::new(CACHE_CAPACITY),
            ttl,
        }
    }

    // TODO: This should return a handle to each caller and delete the key once all handles are dropped.
    pub async fn get_or_compute<F, Fut>(&self, k: K, f: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        let version = self.version(k.clone());
        match self.cache.get_value_or_guard_async(&(k, version)).await {
            Ok(v) => v,
            Err(g) => {
                let v = f().await;
                let _ = g.insert(v.clone());
                v
            }
        }
    }

    fn version(&self, k: K) -> Instant {
        self.latest
            .entry(k.clone())
            .and_modify(|created| {
                let now = Instant::now();
                if now.duration_since(*created) >= self.ttl {
                    // Cache is expired, remove the old one and return a new version for re-creation of the cache.
                    self.remove(&(k, *created));
                    *created = now;
                }
            })
            .or_insert_with(Instant::now)
            .value()
            .clone()
    }

    fn remove(&self, k: &(K, Instant)) {
        self.cache.remove(k);
    }
}

#[cfg(test)]
mod tests {
    use futures::future::join_all;

    use super::*;

    #[tokio::test]
    async fn it_works() {
        let singleflight = Singleflight::new(Duration::from_secs(1));

        let result = join_all(
            (1..=10)
                .into_iter()
                .map(|_| singleflight.get_or_compute(1, || async move { 1 })),
        )
        .await;
        assert_eq!(result, vec![1; 10]);
    }

    #[tokio::test]
    async fn does_the_work_once() {
        let singleflight = Singleflight::new(Duration::from_secs(1));

        let result = join_all(
            (1..=10)
                .into_iter()
                .map(|i| singleflight.get_or_compute(1, move || async move { i })),
        )
        .await;
        assert_eq!(result, vec![1; 10]);
    }

    #[tokio::test]
    async fn it_doesnt_use_values_for_different_keys() {
        let singleflight = Singleflight::new(Duration::from_secs(1));

        let result = join_all(
            (1..=10)
                .into_iter()
                .map(|i| singleflight.get_or_compute(i, move || async move { i })),
        )
        .await;
        assert_eq!(result, (1..=10).into_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn performs_the_operation_if_cache_is_expired() {
        let singleflight = Singleflight::new(Duration::from_millis(10));

        assert_eq!(singleflight.get_or_compute(1, || async move { 1 }).await, 1);
        assert_eq!(
            singleflight
                .get_or_compute(1, || async move { panic!("shouldn't be called") })
                .await,
            1
        );

        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(singleflight.get_or_compute(1, || async move { 42 }).await, 42);
        assert_eq!(
            singleflight
                .get_or_compute(1, || async move { panic!("shouldn't be called") })
                .await,
            42
        );
    }
}
