use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_singleflight::Group;
use futures::future::FutureExt;
use futures::prelude::*;
use hashbrown::HashMap;
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Interval;

const DEFAULT_EXPIRE_DURATION: Duration = Duration::from_secs(180);

pub trait Fetcher<'a, T> {
    type Output: Future<Output = Result<T>>;
    fn call(&self, _: &'a str) -> Self::Output;
}

impl<'a, T, F, Fut> Fetcher<'a, T> for F
where
    T: Send + Clone + PartialEq,
    F: Fn(&'a str) -> Fut,
    Fut: Future<Output = Result<T>> + 'a,
{
    type Output = Fut;
    fn call(&self, s: &'a str) -> Self::Output {
        self(s)
    }
}

pub struct Options<T, F>
where
    T: Send + Clone + PartialEq,
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
    T: Send + Clone + PartialEq,
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
        let ac = AsyncCache {
            inner: Arc::new(AsyncCacheRef {
                options: self,
                sfg: Group::new(),
                data: RwLock::new(HashMap::new()),
            }),
        };
        ac
    }
}

#[derive(Clone)]
pub struct AsyncCache<T, F>
where
    T: Send + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    inner: Arc<AsyncCacheRef<T, F>>,
}

struct Entry<T> {
    val: Option<T>,
    expire: atomic::AtomicBool,
}

impl<T> Entry<T> {
    fn touch(&self) {
        self.expire.store(true, atomic::Ordering::Relaxed);
    }
}

struct AsyncCacheRef<T, F>
where
    T: Send + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    options: Options<T, F>,
    sfg: Group<Option<T>>,
    data: RwLock<HashMap<String, Entry<T>>>,
}

impl<T, F> AsyncCache<T, F>
where
    T: Send + Clone + PartialEq,
    F: for<'a> Fetcher<'a, T>,
{
    /// SetDefault sets the default value of given key if it is new to the cache.
    /// It is useful for cache warming up.
    pub fn set_default(&self, key: &str, value: T) {
        let mut data = self.inner.data.write();
        let ety = Entry {
            val: Some(value),
            expire: atomic::AtomicBool::new(false),
        };
        data.entry(key.to_string()).or_insert(ety);
    }

    /// returns None if first fetch result is err
    pub async fn get(&self, key: &str) -> Option<T> {
        // get value direct from data if exists
        let data = self.inner.data.read();
        let value = data.get(key);
        if value.is_some() {
            value.unwrap().touch();
            return value.unwrap().val.clone();
        }
        drop(data);

        // get data
        let (res, _, _) = self.inner.sfg.work(key, self.first_fetch_impl(key)).await;
        res.unwrap().clone()
    }

    pub fn get_or_set(&self, key: &str, value: T) -> T {
        // get value direct from data if exists
        let data = self.inner.data.read();
        let ety = data.get(key);
        if ety.is_some() && ety.unwrap().val.is_some() {
            ety.unwrap().touch();
            return ety.unwrap().val.clone().unwrap();
        }
        drop(data);

        self.insert_value(key, value.clone());
        value
    }

    pub async fn delete(&self, should_delete: impl Fn(&str) -> bool) {
        let mut data = self.inner.data.write();
        let mut delete_keys = Vec::new();

        for k in data.keys() {
            if should_delete(k) {
                delete_keys.push(k.clone());
            }
        }

        for k in delete_keys {
            let ety = data.remove(&k).unwrap();
            if ety.val.is_some() {
                self.send_delete(k, ety.val.unwrap());
            }
        }
    }

    fn insert_value(&self, key: &str, value: T) {
        // set data
        let ety = Entry {
            val: Some(value),
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
            tx.send((Arc::new(key.to_string()), value));
        }
    }

    async fn first_fetch_impl(&self, key: &str) -> Result<Option<T>> {
        let res = self.inner.options.fetcher.call(key).await;
        if res.is_err() {
            self.send_err(key, res.err().unwrap()).await;
            return Ok(None);
        }

        let value = res.unwrap().clone();
        self.insert_value(key, value.clone());
        Ok(Some(value))
    }

    async fn send_err(&self, key: &str, err: anyhow::Error) {
        let tx = self.inner.options.error_tx.clone();
        if tx.is_some() {
            tx.unwrap().send((Arc::new(key.to_string()), err)).await;
        }
    }

    async fn refresh(&self) {
        let
    }
}

#[cfg(test)]
mod tests {
    use crate::{test_error, Options};
    use anyhow::Result;
    use futures::future::BoxFuture;
    use futures::FutureExt;
    use std::marker::PhantomData;

    async fn test_func(_: &str) -> Result<usize> {
        Ok(1)
    }

    #[tokio::test]
    async fn it_works() {
        let opt = Options {
            refresh_interval: Default::default(),
            expire_interval: Default::default(),
            fetcher: test_func,
            phantom: PhantomData,

            error_tx: None,
            change_tx: None,
            delete_tx: None,
        };

        let opt = Options::new(std::time::Duration::from_secs(5), test_func);
    }
}
