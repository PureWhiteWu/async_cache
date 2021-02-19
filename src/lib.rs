use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::future::FutureExt;
use futures::prelude::*;
use std::marker::PhantomData;

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
    // fetcher
    F: for<'a> Fetcher<'a, T>,
{
    refresh_interval: Duration,
    enable_expire: bool,
    expire_duration: Duration,

    fetcher: F,
    // error_handler: Option<Box<dyn for<'b> Fn(&'b str, anyhow::Error) -> future::BoxFuture<'a, ()>>>,
    // change_handler: Option<Box<dyn for<'b> Fn(&'b str, T, T) -> future::BoxFuture<'a, ()>>>,
    // delete_handler: Option<Box<dyn for<'b> Fn(&'a str, T) -> future::BoxFuture<'a, ()>>>,
    phantom: PhantomData<T>,
}

async fn test_func(_: &str) -> Result<usize> {
    Ok(1)
}

impl<T, F> Options<T, F>
where
    T: Send + Clone + PartialEq,
    // fetcher
    F: for<'a> Fetcher<'a, T>,
{
    // pub fn new(
    //     refresh_interval: Duration,
    //     fetcher: impl Fn(&str) -> Box<dyn Future<Output = Result<T>>>,
    // ) -> Self {
    //     Self {}
    // }
}

// fn change_wrapper<'a, FUT, T>(
//     f: impl Fn(&'a str) -> FUT + 'static,
// ) -> Box<dyn Fn(&'a str) -> futures::future::BoxFuture<'a, Result<T>>>
//     where
//         FUT: Future<Output = ()> + 'a,
// {
//     Box::new(|s: &str| f(s).boxed())
// }

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

async fn test_error(_: &str, e: anyhow::Error) {
    println!("{}", e);
}

#[cfg(test)]
mod tests {
    use crate::{test_error, test_func, Options};
    use anyhow::Result;
    use futures::future::BoxFuture;
    use futures::FutureExt;
    use std::marker::PhantomData;

    #[tokio::test]
    async fn it_works() {
        let opt = Options {
            refresh_interval: Default::default(),
            enable_expire: false,
            expire_duration: Default::default(),
            fetcher: test_func,
            // error_handler: Some(error_wrapper(test_error)),
            // change_handler: None,
            // delete_handler: None,
            phantom: PhantomData,
        };
    }
}
