use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_singleflight::{Group, UnaryGroup};
use futures::future::FutureExt;
use futures::prelude::*;
use hashbrown::HashMap;
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Interval;

const DEFAULT_EXPIRE_DURATION: Duration = Duration::from_secs(180);

#[async_trait::async_trait]
pub trait Fetcher<'a, T>
where
    T: Send + Sync + Clone,
{
    type Error;
    async fn fetch(&self, key: &'a str) -> Result<T>;
}

pub struct Options<T, F>
where
    T: Send + Sync + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    refresh_interval: Duration,
    expire_interval: Option<Duration>,

    fetcher: F,
    phantom: PhantomData<T>,

    error_tx: Option<mpsc::Sender<(Arc<String>, anyhow::Error)>>, // key, error

    change_tx: Option<broadcast::Sender<(Arc<String>, T, T)>>, // key, old, new

    delete_tx: Option<broadcast::Sender<(Arc<String>, T)>>, // key, value
}

impl<T, F> Options<T, F>
where
    T: Send + Sync + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    pub fn new(refresh_interval: Duration, fetcher: F) -> Self {
        Self {
            refresh_interval,
            expire_interval: Some(DEFAULT_EXPIRE_DURATION),
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

    pub fn with_error_tx(mut self, tx: mpsc::Sender<(Arc<String>, anyhow::Error)>) -> Self {
        self.error_tx = Some(tx);
        self
    }

    pub fn with_change_tx(mut self, tx: broadcast::Sender<(Arc<String>, T, T)>) -> Self {
        self.change_tx = Some(tx);
        self
    }

    pub fn with_delete_tx(mut self, tx: broadcast::Sender<(Arc<String>, T)>) -> Self {
        self.delete_tx = Some(tx);
        self
    }

    pub fn build(self) -> AsyncCache<T, F> {
        AsyncCache {
            inner: Arc::new(AsyncCacheRef {
                options: self,
                sfg: Group::new(),
                data: RwLock::new(HashMap::new()),
            }),
        }
    }
}

#[derive(Clone)]
pub struct AsyncCache<T, F>
where
    T: Send + Sync + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    inner: Arc<AsyncCacheRef<T, F>>,
}

struct Entry<T> {
    val: T,
    expire: atomic::AtomicBool,
}

impl<T> Entry<T> {
    fn touch(&self) {
        self.expire.store(true, atomic::Ordering::Relaxed);
    }
}

struct AsyncCacheRef<T, F>
where
    T: Send + Sync + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    options: Options<T, F>,
    sfg: Group<T, anyhow::Error>,
    data: RwLock<HashMap<String, Entry<T>>>,
}

impl<T, F> AsyncCache<T, F>
where
    T: Send + Sync + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    /// SetDefault sets the default value of given key if it is new to the cache.
    /// It is useful for cache warming up.
    pub fn set_default(&self, key: &str, value: T) {
        let mut data = self.inner.data.write();
        let ety = Entry {
            val: value,
            expire: atomic::AtomicBool::new(false),
        };
        data.entry(key.to_string()).or_insert(ety);
    }

    /// returns None if first fetch result is err
    pub async fn get(&self, key: &str) -> Option<T> {
        // get value direct from data if exists
        let data = self.inner.data.read();
        let value = data.get(key);
        if let Some(entry) = value {
            entry.touch();
            return Some(entry.val.clone());
        }
        drop(data);

        // get data
        let fut = self.inner.options.fetcher.fetch(key);
        let (res, e, is_owner) = self.inner.sfg.work(key, fut).await;
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

    pub fn get_or_set(&self, key: &str, value: T) -> T {
        // get value direct from data if exists
        let data = self.inner.data.read();
        let ety = data.get(key);
        if let Some(ety) = ety {
            ety.touch();
            return ety.val.clone();
        }
        drop(data);

        self.insert_value(key, value.clone());
        value
    }

    pub async fn delete(&self, should_delete: impl Fn(&str) -> bool) {
        let mut data = self.inner.data.write();
        let mut delete_keys = Vec::with_capacity(data.keys().len() / 2);

        for k in data.keys() {
            if should_delete(k) {
                delete_keys.push(k.clone());
            }
        }

        for k in delete_keys {
            let ety = data.remove(&k).unwrap();
            self.send_delete(k, ety.val);
        }
    }

    fn insert_value(&self, key: &str, value: T) {
        // set data
        let ety = Entry {
            val: value,
            expire: atomic::AtomicBool::new(false),
        };
        let mut data = self.inner.data.write();
        data.insert(key.to_string(), ety);
        drop(data);
    }

    fn send_delete(&self, key: String, value: T) {
        let tx = &self.inner.options.delete_tx;
        if tx.is_some() {
            let tx = tx.as_ref().unwrap();
            let _ = tx.send((Arc::new(key), value));
        }
    }

    async fn send_err(&self, key: &str, err: anyhow::Error) {
        let tx = self.inner.options.error_tx.clone();
        if let Some(tx) = tx {
            let _ = tx.send((Arc::new(key.to_string()), err)).await;
        }
    }

    async fn refresh(&self) {
        let
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::{test_error, Options};
//     use anyhow::Result;
//     use futures::future::BoxFuture;
//     use futures::FutureExt;
//     use std::marker::PhantomData;

//     async fn test_func(_: &str) -> Result<usize> {
//         Ok(1)
//     }

//     #[tokio::test]
//     async fn it_works() {
//         let opt = Options {
//             refresh_interval: Default::default(),
//             expire_interval: Default::default(),
//             fetcher: test_func,
//             phantom: PhantomData,

//             error_tx: None,
//             change_tx: None,
//             delete_tx: None,
//         };

//         let opt = Options::new(std::time::Duration::from_secs(5), test_func);
//     }
// }
