//! AsyncCache is a cache system that automatically updates and deletes entries using async fetchers.
//!
//! # Usage
//!
//! To use AsyncCache, you need to implement the `Fetcher` trait for the type you want to cache. Then you can use the `Options` struct to configure the cache and create an instance of `AsyncCache`.
//!
//! ```no_run,ignore
//! use async_cache::{DefaultAsyncCache, Fetcher};
//! use std::time::Duration;
//!
//! #[derive(Debug, Clone, PartialEq)]
//! struct MyValue(u32);
//!
//! #[derive(Clone)]
//! struct MyFetcher;
//!
//! impl Fetcher<u64, MyValue> for MyFetcher {
//!     type Error = ();
//!
//!     //// The implementation of fetch function should return the value associated with a key, or an error if it fails.
//!     async fn fetch(&self, key: u64) -> Result<MyValue, Self::Error> {
//!         // Your implementation here
//!         Ok(MyValue(100))
//!     }
//! }
//!
//! let mut cache = DefaultAsyncCache::builder(Duration::from_secs(10), MyFetcher)
//!         .with_expire(Some(Duration::from_secs(10)))
//!         .build();
//!
//! // Now you can use the cache to get and set values
//! let val = cache.get_or_set(42, MyValue(50));
//!
//! assert_eq!(val, MyValue(50));
//!
//! let other_val = cache.get(&42).await.unwrap();
//!
//! assert_eq!(other_val, MyValue(50));
//! ```
//!
//! # Design
//!
//! AsyncCache uses async-singleflight to reduce redundant load on underlying fetcher, requests from different future tasks with the same key will only trigger a single fetcher call.
//!
//! It also provides a few channels to notify the client of the cache when the cache is updated or there was an error with a fetch.
//!
//! `AsyncCache` is thread-safe and can be cloned and shared across tasks.
//!
//! # Example
//!
//! ```no_run,ignore
//! use async_cache::DefaultAsyncCache;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     let interval = Duration::from_millis(100);
//!
//!     let cache = DefaultAsyncCache::builder(interval, GitHubFetcher::new())
//!         .with_expire(Some(Duration::from_secs(30)))
//!         .build();
//!
//!     match cache.get(&42).await {
//!         Some(v) => println!("value: {}", v),
//!         None => println!("first fetch failed"),
//!     }
//!
//!     tokio::time::sleep(Duration::from_secs(5)).await;
//!
//!     match cache.get(&42).await {
//!         Some(v) => println!("value: {}", v),
//!         None => println!("fetch data failed"),
//!     }
//! }
//! ```
use std::future::Future;
use std::hash::{BuildHasher, Hash};
use std::marker::PhantomData;
use std::sync::{atomic, Arc};
use std::time::Duration;

use async_singleflight::Group;
use dashmap::DashMap;
use futures::{prelude::*, stream::FuturesOrdered};
use tokio::sync::{broadcast, mpsc};

const DEFAULT_EXPIRE_DURATION: Duration = Duration::from_secs(180);
const DEFAULT_CACHE_CAPACITY: usize = 16;

pub trait Fetcher<K, T>
where
    T: Send + Sync + 'static,
{
    type Error: Send;

    fn fetch(&self, key: K) -> impl Future<Output = Result<T, Self::Error>> + Send;
}

#[cfg(feature = "ahash")]
type DefaultRandomState = ahash::RandomState;
#[cfg(not(feature = "ahash"))]
type DefaultRandomState = std::hash::RandomState;

pub struct AsyncCacheBuilder<K, T, F, S = DefaultRandomState>
where
    T: Send + Sync + 'static,
    F: Fetcher<K, T>,
{
    refresh_interval: Duration,
    expire_interval: Option<Duration>,
    capacity: usize,
    fetcher: F,

    error_tx: Option<mpsc::Sender<(K, F::Error)>>, // key, error
    delete_tx: Option<broadcast::Sender<(K, T)>>,  // key, value

    _marker: std::marker::PhantomData<S>,
}

impl<K, T, F, S> AsyncCacheBuilder<K, T, F, S>
where
    T: Send + Sync + Clone + 'static,
    F: Fetcher<K, T> + Sync + Send + 'static,
{
    pub fn new(refresh_interval: Duration, fetcher: F) -> Self {
        Self {
            refresh_interval,
            expire_interval: Some(DEFAULT_EXPIRE_DURATION),
            capacity: DEFAULT_CACHE_CAPACITY,
            fetcher,
            error_tx: None,
            delete_tx: None,
            _marker: PhantomData,
        }
    }

    pub fn with_expire(mut self, expire_interval: Option<Duration>) -> Self {
        self.expire_interval = expire_interval;
        self
    }

    pub fn with_error_tx(mut self, tx: mpsc::Sender<(K, F::Error)>) -> Self {
        self.error_tx = Some(tx);
        self
    }

    pub fn with_delete_tx(mut self, tx: broadcast::Sender<(K, T)>) -> Self {
        self.delete_tx = Some(tx);
        self
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    pub fn build(self) -> AsyncCache<K, T, F, S>
    where
        K: Eq + Hash + Sync + Send + Clone + 'static,
        S: BuildHasher + Default + Clone + Sync + Send + 'static,
    {
        let expire_interval = self.expire_interval;
        let refresh_interval = self.refresh_interval;

        let cache = AsyncCache {
            inner: Arc::new(AsyncCacheInner {
                sfg: Group::<K, T, F::Error, S>::new(),
                data: DashMap::with_capacity_and_hasher(self.capacity, S::default()),
                options: AsyncCacheOptions {
                    fetcher: self.fetcher,
                    error_tx: self.error_tx,
                    delete_tx: self.delete_tx,
                },
            }),
        };

        {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache.refresh(refresh_interval).await;
            });
        }

        if let Some(expire_interval) = expire_interval {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache.expire_cron(expire_interval).await;
            });
        }

        cache
    }
}

struct AsyncCacheOptions<K, T, F>
where
    T: Send + Sync + 'static,
    F: Fetcher<K, T>,
{
    fetcher: F,
    error_tx: Option<mpsc::Sender<(K, F::Error)>>, // key, error
    delete_tx: Option<broadcast::Sender<(K, T)>>,  // key, value
}

pub type DefaultAsyncCache<K, T, F> = AsyncCache<K, T, F, DefaultRandomState>;

pub struct AsyncCache<K, T, F, S = DefaultRandomState>
where
    T: Send + Sync + 'static,
    F: Fetcher<K, T> + Sync + Send + 'static,
{
    inner: Arc<AsyncCacheInner<K, T, F, S>>,
}

impl<K, T, F, S> Clone for AsyncCache<K, T, F, S>
where
    T: Send + Sync + 'static,
    F: Fetcher<K, T> + Sync + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct Entry<T> {
    // should always be Some until deleted
    val: Option<T>,
    expire: atomic::AtomicBool,
}

impl<T> Entry<T> {
    #[inline(always)]
    fn new(val: T) -> Self {
        Self {
            val: Some(val),
            expire: atomic::AtomicBool::new(false),
        }
    }

    #[inline(always)]
    fn touch(&self) {
        self.expire.store(false, atomic::Ordering::Relaxed);
    }
}

struct AsyncCacheInner<K, T, F, S = DefaultRandomState>
where
    T: Send + Sync + 'static,
    F: Fetcher<K, T> + Sync + Send,
{
    options: AsyncCacheOptions<K, T, F>,
    sfg: Group<K, T, F::Error, S>,
    data: DashMap<K, Entry<T>, S>,
}

impl<K, T, F, S> AsyncCache<K, T, F, S>
where
    K: Eq + Hash + Sync + Send + Clone,
    F: Fetcher<K, T> + Sync + Send,
    T: Send + Sync + Clone + 'static,
    S: BuildHasher + Clone + 'static,
{
    /// get a builder to customize the cache
    pub fn builder(refresh_interval: Duration, fetcher: F) -> AsyncCacheBuilder<K, T, F, S> {
        AsyncCacheBuilder::new(refresh_interval, fetcher)
    }

    /// SetDefault sets the default value of given key if it is new to the cache.
    /// It is useful for cache warming up.
    pub fn set_default(&self, key: K, value: T) {
        self.inner
            .data
            .entry(key)
            .or_insert_with(|| Entry::new(value));
    }

    /// Returns None if first fetch result is err
    pub async fn get(&self, key: &K) -> Option<T> {
        // get value direct from data if exists
        if let Some(entry) = self.inner.data.get(key) {
            entry.touch();
            debug_assert!(entry.val.is_some());
            return entry.val.clone();
        }

        match self
            .inner
            .sfg
            .work(key, self.inner.options.fetcher.fetch(key.clone()))
            .await
        {
            Ok(value) => {
                self.inner
                    .data
                    .insert(key.clone(), Entry::new(value.clone()));
                Some(value)
            }
            Err(Some(err)) => {
                self.send_error(key.clone(), err).await;
                None
            }
            Err(None) => None,
        }
    }

    pub fn get_or_set(&self, key: K, value: T) -> T {
        let entry = self
            .inner
            .data
            .entry(key)
            .or_insert_with(|| Entry::new(value));

        entry.touch();
        debug_assert!(entry.val.is_some());
        entry.val.clone().unwrap()
    }

    pub fn delete(&self, prediction: impl Fn(&K) -> bool) {
        self.retain(|key, _| !prediction(key));
    }

    pub fn retain(&self, mut prediction: impl FnMut(&K, &mut T) -> bool) {
        self.inner.data.retain(|key, entry| {
            debug_assert!(entry.val.is_some());
            if prediction(key, entry.val.as_mut().unwrap()) {
                true
            } else {
                self.send_delete(key.clone(), entry.val.take());
                false
            }
        });
    }

    fn send_delete(&self, key: K, value: Option<T>) {
        if let Some((tx, value)) = self.inner.options.delete_tx.as_ref().zip(value) {
            let _ = tx.send((key, value));
        }
    }

    async fn send_error(&self, key: K, err: F::Error) {
        if let Some(tx) = self.inner.options.error_tx.as_ref() {
            let _ = tx.send((key, err)).await;
        }
    }

    async fn refresh(&self, refresh_interval: Duration) {
        let mut interval = tokio::time::interval(refresh_interval);

        loop {
            interval.tick().await;

            let mut futures = FuturesOrdered::new();

            let keys: Vec<K> = self
                .inner
                .data
                .iter()
                .map(|entry| {
                    // get all keys, fetch all data using the keys
                    let key = entry.key();
                    let fut = self.inner.options.fetcher.fetch(key.clone());
                    futures.push_back(fut);
                    key.clone()
                })
                .collect();

            debug_assert!(futures.len() == keys.len());

            let mut key_iter = keys.into_iter();
            while let Some((res, key)) = futures.next().await.zip(key_iter.next()) {
                match res {
                    Ok(val) => {
                        self.inner.data.entry(key).and_modify(|entry| {
                            entry.val.replace(val);
                        });
                    }
                    Err(e) => {
                        // only use key when meet error
                        self.send_error(key, e).await;
                    }
                }
            }
        }
    }

    async fn expire_cron(&self, expire_interval: Duration) {
        let mut interval = tokio::time::interval(expire_interval);

        loop {
            interval.tick().await;

            self.inner.data.retain(|key, entry| {
                if entry.expire.load(atomic::Ordering::Relaxed) {
                    // second round, delete expired data
                    self.send_delete(key.clone(), entry.val.take());
                    false
                } else {
                    // first round, mark as expired, but don't delete
                    entry.expire.store(true, atomic::Ordering::Relaxed);
                    true
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faststr::FastStr;
    use std::sync::atomic::Ordering;
    use std::{
        convert::Infallible,
        sync::{atomic::AtomicUsize, Arc},
    };

    #[derive(Clone)]
    struct TestFetcher(Arc<AtomicUsize>);

    impl Fetcher<FastStr, usize> for TestFetcher {
        type Error = Infallible;
        async fn fetch(&self, _: FastStr) -> Result<usize, Self::Error> {
            println!("fetching...");
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(1)
        }
    }

    #[tokio::test]
    async fn it_works() {
        let counter = Arc::new(AtomicUsize::new(0));

        let ac = DefaultAsyncCache::builder(
            std::time::Duration::from_secs(5),
            TestFetcher(counter.clone()),
        )
        .build();

        let first_fetch = ac.get(&"123".into()).await;
        assert_eq!(first_fetch.unwrap(), 1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn expire_works() {
        // async behaviors:
        //  - first fetch (0)
        //  - refresh     (100ms)
        //  - expire      (150ms)
        //  - refresh     (200ms)
        //  - delete      (300ms)
        //
        // so, `println!("fetching...");` is expected to be called 3 times.

        let counter = Arc::new(AtomicUsize::new(0));
        let expire_interval = std::time::Duration::from_millis(150);
        let ac = DefaultAsyncCache::builder(
            std::time::Duration::from_millis(100),
            TestFetcher(counter.clone()),
        )
        .with_expire(Some(expire_interval))
        .build();

        let first_fetch = ac.get(&"123".into()).await;
        assert_eq!(first_fetch.unwrap(), 1);

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }
}
