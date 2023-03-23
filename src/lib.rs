use std::marker::PhantomData;
use std::sync::atomic;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_singleflight::Group;
use faststr::FastStr;
use futures::{prelude::*, stream::FuturesOrdered};
use hashbrown::HashMap;
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};

const DEFAULT_EXPIRE_DURATION: Duration = Duration::from_secs(180);

#[async_trait::async_trait]
pub trait Fetcher<T>
where
    T: Send + Sync + Clone + PartialEq + 'static,
{
    type Error;
    async fn fetch(&self, key: FastStr) -> Result<T>;
}

pub struct Options<T, F>
where
    T: Send + Sync + Clone + PartialEq + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    refresh_interval: Duration,
    expire_interval: Option<Duration>,

    fetcher: F,
    phantom: PhantomData<T>,

    error_tx: Option<mpsc::Sender<(FastStr, anyhow::Error)>>, // key, error

    change_tx: Option<broadcast::Sender<(FastStr, T, T)>>, // key, old, new

    delete_tx: Option<broadcast::Sender<(FastStr, T)>>, // key, value
}

impl<T, F> Options<T, F>
where
    T: Send + Sync + Clone + PartialEq + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
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

    pub fn build(self) -> AsyncCache<T, F> {
        let ac = AsyncCache {
            inner: Arc::new(AsyncCacheRef {
                options: self,
                sfg: Group::new(),
                data: RwLock::new(HashMap::new()),
            }),
        };
        tokio::spawn({
            let ac = ac.clone();
            async move {
                ac.refresh().await;
            }
        });
        ac
    }
}

#[derive(Clone)]
pub struct AsyncCache<T, F>
where
    T: Send + Sync + Clone + PartialEq + 'static,
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
    T: Send + Sync + Clone + PartialEq + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    options: Options<T, F>,
    sfg: Group<T, anyhow::Error>,
    data: RwLock<HashMap<FastStr, Entry<T>>>,
}

impl<T, F> AsyncCache<T, F>
where
    T: Send + Sync + Clone + PartialEq + 'static,
    F: Fetcher<T> + Sync + Send + Clone + 'static,
{
    /// SetDefault sets the default value of given key if it is new to the cache.
    /// It is useful for cache warming up.
    pub fn set_default(&self, key: FastStr, value: T) {
        let mut data = self.inner.data.write();
        let ety = Entry {
            val: value,
            expire: atomic::AtomicBool::new(false),
        };
        data.entry(key).or_insert(ety);
    }

    /// Returns None if first fetch result is err
    pub async fn get(&self, key: FastStr) -> Option<T> {
        // get value direct from data if exists
        {
            let data = self.inner.data.read();
            let value = data.get(&key);
            if let Some(entry) = value {
                entry.touch();
                return Some(entry.val.clone());
            }
            drop(data);
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
        let data = self.inner.data.read();
        let ety = data.get(&key);
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

    fn insert_value(&self, key: FastStr, value: T) {
        // set data
        let ety = Entry {
            val: value,
            expire: atomic::AtomicBool::new(false),
        };
        let mut data = self.inner.data.write();
        data.insert(key, ety);
        drop(data);
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

            let keys: Vec<FastStr>;
            {
                // first, delete expired data
                let mut data = self.inner.data.write();
                let mut delete_keys = Vec::with_capacity(data.keys().len() / 2);
                for (k, v) in data.iter() {
                    if v.expire.load(atomic::Ordering::Relaxed) {
                        delete_keys.push(k.clone());
                    } else {
                        v.expire.store(true, atomic::Ordering::Relaxed);
                    }
                }

                for k in delete_keys {
                    let ety = data.remove(&k).unwrap();
                    self.send_delete(k, ety.val);
                }

                // then, get all keys
                keys = data.keys().cloned().collect();
                drop(data);
            }

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
            let mut data = self.inner.data.write();
            for (k, v) in keys.into_iter().zip(new_data.into_iter()) {
                if let Some(v) = v {
                    data.get_mut(&k).unwrap().val = v;
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
            Ok(1)
        }
    }

    #[tokio::test]
    async fn it_works() {
        let ac = Options::new(std::time::Duration::from_secs(5), TestFetcher).build();
        let first_fetch = ac.get("123".into()).await;
        assert_eq!(first_fetch.unwrap(), 1);
    }
}
