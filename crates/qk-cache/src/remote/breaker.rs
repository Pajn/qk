//! Leaves a store out of the rest of a run once it has stalled twice: a store
//! that stops answering would otherwise cost every task that asks it the
//! whole stall.
//!
//! Once it is left out, a lookup finds nothing, so a task misses quietly
//! rather than reporting the store each time, and an upload fails at once.
//! A stall counts whether it comes before the answer or while its body is
//! read.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use super::index;
use super::store::{Body, Framing, Object, Store};

/// Stalls after which the store is left out.
const STALLS: usize = 2;

/// A request that received nothing from the store for too long.
#[derive(Debug)]
pub(super) struct Stalled(pub std::time::Duration);

impl std::fmt::Display for Stalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the store sent nothing for {}s",
            self.0.as_secs_f64()
        )
    }
}

impl std::error::Error for Stalled {}

/// Why a request was not made.
#[derive(Debug)]
struct LeftOut;

impl std::fmt::Display for LeftOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the store stalled {STALLS} times, so this run asks it nothing more"
        )
    }
}

impl std::error::Error for LeftOut {}

/// Whether `error` is a stall, as the request or the read of its body
/// failed with it.
fn stalled(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if error.is::<Stalled>() {
            return true;
        }
        let inner = match error.downcast_ref::<ureq::Error>() {
            Some(ureq::Error::Io(io)) => io.get_ref().map(|inner| inner as _),
            _ => match error.downcast_ref::<std::io::Error>() {
                Some(io) => io.get_ref().map(|inner| inner as _),
                None => None,
            },
        };
        cause = inner.or_else(|| error.source());
    }
    false
}

/// The stalls a store's requests have made.
#[derive(Default)]
struct Stalls(AtomicUsize);

impl Stalls {
    fn open(&self) -> bool {
        self.0.load(Ordering::SeqCst) >= STALLS
    }

    /// Counts a stall, saying so when it leaves the store out.
    fn add(&self) {
        if self.0.fetch_add(1, Ordering::SeqCst) + 1 == STALLS {
            qk_executor::status!("qk: remote cache left out: {LeftOut}");
        }
    }
}

/// A store, left out once its requests have stalled [`STALLS`] times.
pub(super) struct Breaker {
    store: Box<dyn Store>,
    stalls: Arc<Stalls>,
}

/// A body whose read stalling counts toward its store's stalls.
struct Counted {
    body: Body,
    stalls: Arc<Stalls>,
}

impl Read for Counted {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.body.read(buffer);
        if let Err(error) = &read
            && stalled(error)
        {
            self.stalls.add();
        }
        read
    }
}

impl Breaker {
    pub(super) fn new(store: Box<dyn Store>) -> Self {
        Breaker {
            store,
            stalls: Arc::default(),
        }
    }

    /// `request`'s answer, or `closed` once the store is left out.
    fn ask<T>(
        &self,
        closed: impl FnOnce() -> Result<T>,
        request: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if self.stalls.open() {
            return closed();
        }
        let answer = request();
        if let Err(error) = &answer
            && stalled(error.as_ref())
        {
            self.stalls.add();
        }
        answer
    }

    /// A body as [`ask`](Self::ask) answers it, its reads counted too.
    fn counted(&self, body: Result<Option<Body>>) -> Result<Option<Body>> {
        Ok(body?.map(|body| {
            Box::new(Counted {
                body,
                stalls: self.stalls.clone(),
            }) as Body
        }))
    }
}

fn refused<T>() -> Result<T> {
    Err(LeftOut.into())
}

impl Store for Breaker {
    fn framing(&self) -> Framing {
        self.store.framing()
    }

    fn begin(&self, root: &Path) {
        self.store.begin(root);
    }

    fn get_blob(&self, hash: &str, owner: Object, task: &str) -> Result<Option<Body>> {
        let body = self.ask(|| Ok(None), || self.store.get_blob(hash, owner, task));
        self.counted(body)
    }

    fn location(&self) -> String {
        self.store.location()
    }

    fn sync_index(&self, known: &mut index::Known) -> Result<()> {
        self.ask(refused, || self.store.sync_index(known))
    }

    fn get(&self, object: Object, task: &str) -> Result<Option<Body>> {
        let body = self.ask(|| Ok(None), || self.store.get(object, task));
        self.counted(body)
    }

    fn has(&self, object: Object) -> Result<bool> {
        self.ask(|| Ok(false), || self.store.has(object))
    }

    fn put(&self, object: Object, file: File) -> Result<()> {
        self.ask(refused, || self.store.put(object, file))
    }

    fn announce(&self, key: &str, apart: &[String]) -> Result<Option<String>> {
        self.ask(refused, || self.store.announce(key, apart))
    }

    fn list(
        &self,
        keys: BTreeSet<String>,
        apart: HashMap<String, Vec<String>>,
        members: Vec<String>,
        known: Option<&index::Known>,
        announcements: BTreeSet<String>,
    ) -> Result<()> {
        self.ask(refused, || {
            self.store.list(keys, apart, members, known, announcements)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store whose every request stalls, counting them.
    struct Silent(Arc<AtomicUsize>);

    impl Silent {
        fn stall<T>(&self) -> Result<T> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                Stalled(std::time::Duration::from_secs(30)),
            ))
            .into())
        }
    }

    impl Store for Silent {
        fn location(&self) -> String {
            "silent".into()
        }
        fn sync_index(&self, _: &mut index::Known) -> Result<()> {
            self.stall()
        }
        fn get(&self, _: Object, _: &str) -> Result<Option<Body>> {
            self.stall()
        }
        fn has(&self, _: Object) -> Result<bool> {
            self.stall()
        }
        fn put(&self, _: Object, _: File) -> Result<()> {
            self.stall()
        }
        fn announce(&self, _: &str, _: &[String]) -> Result<Option<String>> {
            self.stall()
        }
        fn list(
            &self,
            _: BTreeSet<String>,
            _: HashMap<String, Vec<String>>,
            _: Vec<String>,
            _: Option<&index::Known>,
            _: BTreeSet<String>,
        ) -> Result<()> {
            self.stall()
        }
    }

    #[test]
    fn a_store_that_stalled_twice_is_asked_nothing_more() {
        let asked = Arc::new(AtomicUsize::new(0));
        let breaker = Breaker::new(Box::new(Silent(asked.clone())));
        assert!(breaker.get(Object::Entry("a"), "task").is_err());
        assert!(breaker.has(Object::Entry("a")).is_err());
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        // Left out: lookups find nothing, and uploads fail, without asking.
        assert!(breaker.get(Object::Entry("a"), "task").unwrap().is_none());
        assert!(!breaker.has(Object::Entry("a")).unwrap());
        let file = tempfile::tempfile().unwrap();
        let refused = breaker.put(Object::Entry("a"), file).unwrap_err();
        assert!(
            refused.to_string().contains("asks it nothing more"),
            "{refused}"
        );
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_body_that_stalls_counts_as_a_stall() {
        struct Stalling;
        impl Read for Stalling {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    Stalled(std::time::Duration::from_secs(30)),
                ))
            }
        }
        let stalls = Arc::new(Stalls::default());
        for _ in 0..STALLS {
            let mut body = Counted {
                body: Box::new(Stalling),
                stalls: stalls.clone(),
            };
            assert!(body.read(&mut [0; 8]).is_err());
        }
        assert!(stalls.open());
    }

    #[test]
    fn other_failures_leave_the_store_in() {
        struct Refusing;
        impl Store for Refusing {
            fn location(&self) -> String {
                "refusing".into()
            }
            fn sync_index(&self, _: &mut index::Known) -> Result<()> {
                Ok(())
            }
            fn get(&self, _: Object, _: &str) -> Result<Option<Body>> {
                Err(ureq::Error::Io(std::io::ErrorKind::ConnectionReset.into()).into())
            }
            fn has(&self, _: Object) -> Result<bool> {
                Ok(true)
            }
            fn put(&self, _: Object, _: File) -> Result<()> {
                Ok(())
            }
            fn announce(&self, _: &str, _: &[String]) -> Result<Option<String>> {
                Ok(None)
            }
            fn list(
                &self,
                _: BTreeSet<String>,
                _: HashMap<String, Vec<String>>,
                _: Vec<String>,
                _: Option<&index::Known>,
                _: BTreeSet<String>,
            ) -> Result<()> {
                Ok(())
            }
        }
        let breaker = Breaker::new(Box::new(Refusing));
        for _ in 0..STALLS + 1 {
            assert!(breaker.get(Object::Entry("a"), "task").is_err());
        }
        assert!(breaker.has(Object::Entry("a")).unwrap());
    }
}
