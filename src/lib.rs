use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::future::FutureExt;
use futures::prelude::*;

pub struct Options<T, FUT, F>
where
    T: Send + Clone + PartialEq,
    FUT: Future<Output = Result<T>> + Send,
    // fetcher
    F: for<'a> Fn(&'a str) -> FUT,
{
    refresh_interval: Duration,
    enable_expire: bool,
    expire_duration: Duration,

    fetcher: F,

    error_handler: Option<Box<dyn for<'a> Fn(&'a str, anyhow::Error) -> future::BoxFuture<'a, ()>>>,
    change_handler: Option<Box<dyn for<'a> Fn(&'a str, T, T) -> future::BoxFuture<'a, ()>>>,
    delete_handler: Option<Box<dyn for<'a> Fn(&'a str, T) -> future::BoxFuture<'a, ()>>>,
}

async fn test_func(_: &str) -> Result<usize> {
    Ok(1)
}

impl<T, FUT, F> Options<T, FUT, F>
where
    T: Send + Clone + PartialEq,
    FUT: Future<Output = Result<T>> + Send + 'static,
    // fetcher
    F: Fn(&str) -> FUT,
{
    // pub fn new(
    //     refresh_interval: Duration,
    //     fetcher: impl Fn(&str) -> Box<dyn Future<Output = Result<T>>>,
    // ) -> Self {
    //     Self {}
    // }
}

fn wrapper<'a, FUT, T>(
    f: impl Fn(&'a str) -> FUT + 'static,
) -> Box<dyn Fn(&'a str) -> futures::future::BoxFuture<'a, Result<T>>>
where
    FUT: Future<Output = Result<T>> + Send + 'a,
{
    Box::new(|s: &str| f(s).boxed())
}

// #[derive(Clone)]
// pub struct AsyncCache<T>
// where
//     T: Send + Clone + PartialEq,
// {
//     inner: Arc<AsyncCacheRef<T>>,
// }
//
// struct AsyncCacheRef<T>
// where
//     T: Send + Clone + PartialEq,
// {
//     options: Options<T>,
// }
//
// impl<T> AsyncCacheRef<T>
// where
//     T: Send + Clone + PartialEq,
// {
//     // pub fn new(options: Options<T>) -> AsyncCacheRef<T> {}
// }

#[cfg(test)]
mod tests {
    use crate::{test_func, wrapper, Options};
    use anyhow::Result;
    use futures::future::BoxFuture;
    use futures::FutureExt;

    #[tokio::test]
    async fn it_works() {
        let opt = Options {
            refresh_interval: Default::default(),
            enable_expire: false,
            expire_duration: Default::default(),
            fetcher: wrapper(test_func),
            error_handler: None,
            change_handler: None,
            delete_handler: None,
        };
    }
}
