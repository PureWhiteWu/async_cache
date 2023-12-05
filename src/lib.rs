//! AsyncCache is a cache system that automatically updates and deletes entries using async fetchers.
//!
//! # Usage
//!
//! To use AsyncCache, you need to implement the `Fetcher` trait for the type you want to cache. Then you can use the `Options` struct to configure the cache and create an instance of `AsyncCache`.
//!
//! ```no_run,ignore
//! use async_cache::{AsyncCache, Fetcher, Options};
//! use faststr::FastStr;
//! use std::time::Duration;
//! use dashmap::DashMap;
//!
//! #[derive(Clone)]
//! struct MyValue(u32);
//!
//! #[derive(Clone)]
//! struct MyFetcher;
//!
//! #[async_trait::async_trait]
//! impl Fetcher<MyValue> for MyFetcher {
//!     type Error = anyhow::Error;
//!
//!     //// The implementation of fetch function should return the value associated with a key, or an error if it fails.
//!     async fn fetch(&self, key: FastStr) -> Result<MyValue, Self::Error> {
//!         // Your implementation here
//!         Ok(MyValue(100))
//!     }
//! }
//!
//! let mut cache =
//!     Options::new(Duration::from_secs(10), MyFetcher).with_expire(Some(Duration::from_secs(10)))
//!         .build();
//!
//! // Now you can use the cache to get and set values
//! let key = FastStr::from("key");
//! let val = cache.get_or_set(key.clone(), MyValue(50));
//!
//! assert_eq!(val, MyValue(50));
//!
//! let other_val = cache.get(key).await.unwrap();
//!
//! assert_eq!(other_val, MyValue(50));
//!
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
//! ```ignore
//! use async_cache::{AsyncCache, Options};
//! use faststr::FastStr;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     let interval = Duration::from_millis(100);
//!
//!     let options = Options::new(interval, GitHubFetcher::new())
//!         .with_expire(Some(Duration::from_secs(30)));
//!
//!     let cache = options.build();
//!
//!     let key = FastStr::from("your key");
//!
//!     match cache.get(key).await {
//!         Some(v) => println!("value: {}", v),
//!         None => println!("first fetch failed"),
//!     }
//!
//!     tokio::time::delay_for(Duration::from_secs(5)).await;
//!
//!     match cache.get(key).await {
//!         Some(v) => println!("value: {}", v),
//!         None => println!("fetch data failed"),
//!     }
//! }
//! ```

use std::marker::PhantomData;
use std::sync::atomic;
use std::sync::Arc;
use std::time::Duration;

use ahash;
use anyhow::Result;
use async_singleflight::Group;
use dashmap::DashMap;
use faststr::FastStr;
use futures::{prelude::*, stream::FuturesOrdered};
use tokio::sync::{broadcast, mpsc};

const DEFAULT_EXPIRE_DURATION: Duration = Duration::from_secs(180);
const DEFAULT_CACHE_CAPACITY: usize = 16;

#[async_trait::async_trait]
pub trait Fetcher<T>
where
    T: Send + Sync + 'static,
{
    type Error;
    async fn fetch(&self, key: FastStr) -> Result<T>;
}

pub struct Options<T, F>
where
    T: Send + Sync + Clone + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    refresh_interval: Duration,
    expire_interval: Option<Duration>,
    capacity: usize,

    fetcher: F,
    phantom: PhantomData<T>,

    error_tx: Option<mpsc::Sender<(FastStr, anyhow::Error)>>, // key, error

    change_tx: Option<broadcast::Sender<(FastStr, T, T)>>, // key, old, new

    delete_tx: Option<broadcast::Sender<(FastStr, T)>>, // key, value
}

impl<T, F> Options<T, F>
where
    T: Send + Sync + Clone + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    pub fn new(refresh_interval: Duration, fetcher: F) -> Self {
        Self {
            refresh_interval,
            expire_interval: Some(DEFAULT_EXPIRE_DURATION),
            capacity: DEFAULT_CACHE_CAPACITY,
            fetcher,
            phantom: PhantomData,
            error_tx: None,
            change_tx: None,
            delete_tx: None,
        }
    }

    pub fn with_expire(mut self, expire_interval: Option<Duration>) -> Self {
        self.expire_interval = expire_interval;
        self
    }

    pub fn with_error_tx(mut self, tx: mpsc::Sender<(FastStr, anyhow::Error)>) -> Self {
        self.error_tx = Some(tx);
        self
    }

    pub fn with_change_tx(mut self, tx: broadcast::Sender<(FastStr, T, T)>) -> Self {
        self.change_tx = Some(tx);
        self
    }

    pub fn with_delete_tx(mut self, tx: broadcast::Sender<(FastStr, T)>) -> Self {
        self.delete_tx = Some(tx);
        self
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    pub fn build(self) -> AsyncCache<T, F> {
        let ac = AsyncCache {
            inner: Arc::new(AsyncCacheRef {
                sfg: Group::new(),
                data: DashMap::with_capacity_and_hasher(
                    self.capacity,
                    ahash::RandomState::default(),
                ),
                options: self, // move options into AsyncCacheRef, so this should be the last line using self
            }),
        };

        tokio::spawn({
            let ac = ac.clone();
            async move {
                ac.refresh().await;
            }
        });

        if ac.is_expiration_enabled() {
            tokio::spawn({
                let ac = ac.clone();
                async move {
                    ac.expire_cron().await;
                }
            });
        }
        ac
    }
}

#[derive(Clone)]
pub struct AsyncCache<T, F>
where
    T: Send + Sync + Clone + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    inner: Arc<AsyncCacheRef<T, F>>,
}

struct Entry<T> {
    val: T,
    expire: atomic::AtomicBool,
}

impl<T> Entry<T> {
    fn touch(&self) {
        self.expire.store(false, atomic::Ordering::Relaxed);
    }
}

struct AsyncCacheRef<T, F>
where
    T: Send + Sync + Clone + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    options: Options<T, F>,
    sfg: Group<T, anyhow::Error>,
    data: DashMap<FastStr, Entry<T>, ahash::RandomState>,
}

impl<T, F> AsyncCache<T, F>
where
    T: Send + Sync + Clone + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    /// SetDefault sets the default value of given key if it is new to the cache.
    /// It is useful for cache warming up.
    pub fn set_default(&self, key: FastStr, value: T) {
        let ety = Entry {
            val: value,
            expire: atomic::AtomicBool::new(false),
        };
        self.inner.data.entry(key).or_insert(ety);
    }

    /// Returns None if first fetch result is err
    pub async fn get(&self, key: FastStr) -> Option<T> {
        // get value direct from data if exists
        {
            let value = self.inner.data.get(&key);
            if let Some(entry) = value {
                entry.touch();
                return Some(entry.val.clone());
            }
        }

        // get data
        let fut = self.inner.options.fetcher.fetch(key.clone());
        let (res, e, is_owner) = self.inner.sfg.work(&key, fut).await;
        if is_owner {
            if let Some(e) = e {
                self.send_err(key, e).await;
                return None;
            }
            let value = res.clone().unwrap();
            self.insert_value(key, value);
        }
        res
    }

    pub fn get_or_set(&self, key: FastStr, value: T) -> T {
        // get value directly from data if exists
        let ety = self.inner.data.get(&key);
        if let Some(ety) = ety {
            ety.touch();
            return ety.val.clone();
        }

        self.insert_value(key, value.clone());
        value
    }

    pub async fn delete(&self, should_delete: impl Fn(&str) -> bool) {
        let delete_keys = self
            .inner
            .data
            .iter()
            .filter(|ref_mul| should_delete(ref_mul.key()))
            .map(|kv_ref| kv_ref.key().clone())
            .collect::<Vec<_>>();

        for k in delete_keys {
            if let Some((_, ety)) = self.inner.data.remove(&k) {
                self.send_delete(k, ety.val);
            }
        }
    }

    fn insert_value(&self, key: FastStr, value: T) {
        // set data
        let ety = Entry {
            val: value,
            expire: atomic::AtomicBool::new(false),
        };
        self.inner.data.insert(key, ety);
    }

    fn send_delete(&self, key: FastStr, value: T) {
        let tx = &self.inner.options.delete_tx;
        if tx.is_some() {
            let tx = tx.as_ref().unwrap();
            let _ = tx.send((key, value));
        }
    }

    async fn send_err(&self, key: FastStr, err: anyhow::Error) {
        let tx = self.inner.options.error_tx.clone();
        if let Some(tx) = tx {
            let _ = tx.send((key, err)).await;
        }
    }

    async fn refresh(&self) {
        let mut interval = tokio::time::interval(self.inner.options.refresh_interval);

        loop {
            interval.tick().await;

            // first, get all keys
            let keys: Vec<FastStr> = self
                .inner
                .data
                .iter()
                .map(|kv_ref| kv_ref.key().clone())
                .collect();

            // after that, fetch all data using the keys
            let mut futures = FuturesOrdered::new();
            for k in &keys {
                let fut = self.inner.options.fetcher.fetch(k.clone());
                futures.push_back(fut);
            }

            // and save them into a new vec
            let mut new_data = Vec::with_capacity(futures.len());
            assert!(futures.len() == keys.len());
            let mut key_index = 0;
            while let Some(res) = futures.next().await {
                let key = unsafe { keys.get_unchecked(key_index) }.clone();
                key_index += 1;
                match res {
                    Ok(v) => new_data.push(Some(v)),
                    Err(e) => {
                        self.send_err(key, e).await;
                        new_data.push(None);
                    }
                }
            }

            // finally, replace the old data with the new one
            for (k, v) in keys.into_iter().zip(new_data.into_iter()) {
                if let Some(v) = v {
                    if let Some(mut old) = self.inner.data.get_mut(&k) {
                        old.val = v;
                    }
                }
            }
        }
    }
    fn is_expiration_enabled(&self) -> bool {
        self.inner.options.expire_interval.is_some()
    }

    async fn expire_cron(&self) {
        let mut interval = tokio::time::interval(self.inner.options.expire_interval.unwrap());

        let mut delete_keys = Vec::with_capacity(self.inner.data.len() / 2);
        loop {
            interval.tick().await;

            delete_keys.clear();
            for kv_ref in self.inner.data.iter() {
                if kv_ref.value().expire.load(atomic::Ordering::Relaxed) {
                    delete_keys.push(kv_ref.key().clone());
                } else {
                    // first round, mark as expired
                    kv_ref.value().expire.store(true, atomic::Ordering::Relaxed);
                }
            }

            // second round, delete expired data
            for k in delete_keys.iter() {
                if let Some((_, ety)) = self.inner.data.remove(k) {
                    self.send_delete(k.clone(), ety.val);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Options;
    use anyhow::Result;
    use faststr::FastStr;
    use std::convert::Infallible;

    #[derive(Clone)]
    struct TestFetcher;

    #[async_trait::async_trait]
    impl crate::Fetcher<usize> for TestFetcher {
        type Error = Infallible;
        async fn fetch(&self, _: FastStr) -> Result<usize> {
            println!("fetching...");
            Ok(1)
        }
    }

    #[tokio::test]
    async fn it_works() {
        let ac = Options::new(std::time::Duration::from_secs(5), TestFetcher).build();
        let first_fetch = ac.get("123".into()).await;
        assert_eq!(first_fetch.unwrap(), 1);
    }

    #[tokio::test]
    async fn expire_works() {
        // async behaviors: first fetch(0s), refresh (2s), expire(3s), refresh(4s), delete(6s)
        // so, `println!("fetching...");` is expected to be called three times.
        let expire_interval = std::time::Duration::from_secs(3);
        let ac = Options::new(std::time::Duration::from_secs(2), TestFetcher)
            .with_expire(Some(expire_interval))
            .build();
        let first_fetch = ac.get("123".into()).await;
        assert_eq!(first_fetch.unwrap(), 1);
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
