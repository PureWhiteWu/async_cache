use std::sync::Arc;

#[derive(Clone)]
struct AsyncCache<T> {
    inner: Arc<AsyncCacheRef<T>>,
}

struct AsyncCacheRef<T> {}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
