use std::future::Future;
use std::hash::Hash;

use quick_cache::sync::Cache;

const CACHE_CAPACITY: usize = 1024;

// Singleflight allows deduplication of async work for a given key.
#[derive(Debug)]
pub struct Singleflight<K, V> {
    work: Cache<K, V>,
}

impl<K: Hash + Eq + Clone, V: Clone> Singleflight<K, V> {
    pub fn new() -> Singleflight<K, V> {
        Singleflight {
            work: Cache::new(CACHE_CAPACITY),
        }
    }

    pub fn remove(&self, k: K) {
        self.work.remove(&k);
    }

    pub async fn get_or_compute<F, Fut>(&self, k: K, f: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        match self.work.get_value_or_guard_async(&k).await {
            Ok(v) => v,
            Err(g) => {
                let v = f().await;
                let _ = g.insert(v.clone());
                v
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::future::join_all;

    use super::*;

    #[tokio::test]
    async fn it_works() {
        let singleflight = Singleflight::new();

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
        let singleflight = Singleflight::new();

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
        let singleflight = Singleflight::new();

        let result = join_all(
            (1..=10)
                .into_iter()
                .map(|i| singleflight.get_or_compute(i, move || async move { i })),
        )
        .await;
        assert_eq!(result, (1..=10).into_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn deletes_the_key() {
        let singleflight = Singleflight::new();

        assert_eq!(singleflight.get_or_compute(1, || async move { 1 }).await, 1);
        assert_eq!(singleflight.get_or_compute(1, || async move { 2 }).await, 1);

        singleflight.remove(1);

        assert_eq!(singleflight.get_or_compute(1, || async move { 42 }).await, 42);
    }
}
