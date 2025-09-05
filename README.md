# Async Cache Library

This library provides an async cache implementation that can be used to store and retrieve data with an expiration time. It can store data in memory and can asynchronously fetch data using a custom data fetcher.

# API Overview

```rust
pub trait Fetcher<K, T>
where
    T: Send + Sync + Clone + 'static,
{
    type Error: Send;

    async fn fetch(&self, key: K) -> Result<T, Self::Error>;
}
```

```rust
pub struct AsyncCacheBuilder<K, T, F, S = DefaultRandomState>;

impl<K, T, F, S> AsyncCacheBuilder<K, T, F, S>
where
    T: Send + Sync + Clone + 'static,
    F: Fetcher<K, T> + Sync + Send + 'static,
{
    pub fn new(refresh_interval: Duration, fetcher: F) -> Self;

    pub fn with_expire(mut self, expire_interval: Option<Duration>) -> Self;

    pub fn with_error_tx(mut self, tx: mpsc::Sender<(K, F::Error)>) -> Self;

    pub fn with_delete_tx(mut self, tx: broadcast::Sender<(K, T)>) -> Self;

    pub fn build(self) -> AsyncCache<K, T, F, S>
}
```

```rust
pub struct AsyncCache<K, T, F, S = DefaultRandomState>;

impl<K, T, F, S> AsyncCache<K, T, F, S>
where
    K: Eq + Hash + Sync + Send + Clone,
    F: Fetcher<K, T> + Sync + Send,
    T: Send + Sync + Clone + 'static,
    S: BuildHasher + Clone + 'static,
{
    pub fn builder(refresh_interval: Duration, fetcher: F) -> AsyncCacheBuilder<K, T, F, S>;

    pub fn set_default(&self, key: K, value: T);

    pub async fn get(&self, key: K) -> Option<T>;
    pub fn get_or_set(&self, key: &K, value: T) -> T;

    pub fn delete(&self, prediction: impl Fn(&K) -> bool);
    pub fn retain(&self, prediction: impl Fn(&K, &mut T) -> bool);
}
```

# Guide

To use AsyncCache, you need to implement the `Fetcher` trait for the type you want to cache. Then you can use the `AsyncCacheBuilder` struct to configure the cache and create an instance of `AsyncCache`.

Create an instance of `AsyncCache` using the `AsyncCacheBuilder` struct.

```rust
let builder = AsyncCache::builder(Duration::from_secs(60), YourFetcher);
let cache = builder.build();
```

Then, you can interact with the cache by using its methods: `set_default`, `get`, `get_or_set`, and `delete`.

## set_default

Here's an example of how to set a default value for a key:

```rust
cache.set_default(FastStr::from("key"), "default_value".to_string());
```

This sets the default value for the key "key" to "default_value".

## get

Getting a value from the cache is done asynchronously using the `get` method. It returns an `Option<T>` where `T` is the type of the value in the cache. If the key was not found, it will try to fetch it, and if the first fetch fails, it returns `None`.

```rust
let value = cache.get(FastStr::from("key")).await.unwrap();
```

This gets the value for the key "key" from the cache.

## get_or_set

If you want to set a value for a key if it's not found in the cache, you can use the `get_or_set` method:

```rust
let value = cache.get_or_set(FastStr::from("key"), "default_value".to_string());
```

This gets the value for the key "key" from the cache. If the value was not found, it sets it to "default_value". Either way, it returns the value of the key.

## delete

If you want to delete data from the cache, you can use the `delete` method.

```rust
cache.delete(|key| key.starts_with("prefix_")).await;
```

This deletes all keys that start with the prefix "prefix\_" from the cache.

## Builder

The `AsyncCacheBuilder` struct is used to configure the `AsyncCache`. It takes in the `Fetcher` trait, the refresh interval, and an optional expire interval. You can also pass in mpsc channels to receive errors, changes to values, or deletions.

```rust
let builder = AsyncCache::builder(Duration::from_secs(60), YourFetcher)
    .with_expire(Some(Duration::from_secs(60)))
    .with_error_tx(tx)
    .with_delete_tx(tx);
```

`with_expire` sets the expire interval for the cache. If this is not set, the default expire interval is 180 seconds.

`with_error_tx` sets the mpsc channel for receiving errors.

`with_delete_tx` sets the broadcast channel for receiving deletions.

# License

The async cache library is licensed under MIT OR Apache-2.0.
