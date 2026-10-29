//! Fetching ahead: a few workers fetch, in the order given, what tasks are
//! likely to ask for once they start, so that a task finds it fetched.
//!
//! Each item is fetched for a key, and only a task asking with that key
//! takes it. A task that asks while its item is still queued takes it off
//! the queue and fetches for itself, rather than wait behind the others; one
//! that asks while it is being fetched waits for that fetch. An item is taken
//! once. Stopping drops what is still queued; what is being fetched runs on,
//! and is waited for or left, as the caller says.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// What taking an item came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Taken<T> {
    /// It was fetched, and found this.
    Found(T),
    /// It was fetched, and found nothing.
    Missing,
    /// Nothing was fetched for this key, or it was taken already: the task
    /// fetches for itself.
    Nothing,
}

enum Stage<J, T> {
    Queued(J),
    Fetching,
    Done(Option<T>),
    Taken,
}

struct Slot<J, T> {
    key: Vec<String>,
    stage: Stage<J, T>,
}

struct State<J, T> {
    queue: VecDeque<String>,
    slots: HashMap<String, Slot<J, T>>,
    stopped: bool,
}

struct Shared<J, T> {
    state: Mutex<State<J, T>>,
    changed: Condvar,
}

pub(crate) struct Ahead<J, T> {
    shared: Arc<Shared<J, T>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl<J: Send + 'static, T: Send + 'static> Ahead<J, T> {
    /// Starts `workers` threads fetching `items`, each an id, the key it is
    /// fetched for and what `fetch` needs, in order. An id given twice keeps
    /// its first.
    pub(crate) fn start(
        items: Vec<(String, Vec<String>, J)>,
        workers: usize,
        fetch: impl Fn(J) -> Option<T> + Send + Sync + 'static,
    ) -> Self {
        let mut queue = VecDeque::new();
        let mut slots = HashMap::new();
        for (id, key, job) in items {
            if slots.contains_key(&id) {
                continue;
            }
            queue.push_back(id.clone());
            slots.insert(
                id,
                Slot {
                    key,
                    stage: Stage::Queued(job),
                },
            );
        }
        let count = workers.min(queue.len());
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue,
                slots,
                stopped: false,
            }),
            changed: Condvar::new(),
        });
        let fetch = Arc::new(fetch);
        let workers = (0..count)
            .map(|_| {
                let (shared, fetch) = (shared.clone(), fetch.clone());
                std::thread::spawn(move || work(&shared, &*fetch))
            })
            .collect();
        Self {
            shared,
            workers: Mutex::new(workers),
        }
    }

    /// Takes the item `id` fetched for `key`, waiting while it is fetched.
    pub(crate) fn take(&self, id: &str, key: &[String]) -> Taken<T> {
        let mut state = self.shared.state.lock().unwrap();
        loop {
            let Some(slot) = state.slots.get_mut(id) else {
                return Taken::Nothing;
            };
            // Fetched for another key, it is no use to this task, which does
            // not wait for it.
            if slot.key != key {
                if matches!(slot.stage, Stage::Queued(_)) {
                    slot.stage = Stage::Taken;
                }
                return Taken::Nothing;
            }
            match std::mem::replace(&mut slot.stage, Stage::Taken) {
                Stage::Queued(_) | Stage::Taken => return Taken::Nothing,
                Stage::Done(Some(found)) => return Taken::Found(found),
                Stage::Done(None) => return Taken::Missing,
                Stage::Fetching => {
                    slot.stage = Stage::Fetching;
                    state = self.shared.changed.wait(state).unwrap();
                }
            }
        }
    }

    /// Drops what is still queued, and waits for what is being fetched when
    /// `wait` says to; otherwise its workers are left to finish on their own.
    pub(crate) fn stop(&self, wait: bool) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.stopped = true;
            state.queue.clear();
        }
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        if wait {
            for worker in workers {
                let _ = worker.join();
            }
        }
    }
}

fn work<J, T>(shared: &Shared<J, T>, fetch: &(impl Fn(J) -> Option<T> + ?Sized)) {
    loop {
        let (id, job) = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if state.stopped {
                    return;
                }
                let Some(id) = state.queue.pop_front() else {
                    return;
                };
                let slot = state.slots.get_mut(&id).expect("queued items have slots");
                // Taken off the queue by a task that asked first.
                if let Stage::Queued(_) = slot.stage {
                    let Stage::Queued(job) = std::mem::replace(&mut slot.stage, Stage::Fetching)
                    else {
                        unreachable!()
                    };
                    break (id, job);
                }
            }
        };
        // A fetch that panics found nothing, rather than leave a task
        // waiting for it.
        let found =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fetch(job))).unwrap_or(None);
        let mut state = shared.state.lock().unwrap();
        if let Some(slot) = state.slots.get_mut(&id)
            && matches!(slot.stage, Stage::Fetching)
        {
            slot.stage = Stage::Done(found);
        }
        shared.changed.notify_all();
    }
}

#[cfg(test)]
impl<J, T> Ahead<J, T> {
    /// Waits until nothing is queued or being fetched.
    fn settle(&self) {
        let mut state = self.shared.state.lock().unwrap();
        while state
            .slots
            .values()
            .any(|slot| matches!(slot.stage, Stage::Queued(_) | Stage::Fetching))
        {
            state = self.shared.changed.wait(state).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    fn key(text: &str) -> Vec<String> {
        vec![text.to_owned()]
    }

    #[test]
    fn fetches_in_order_and_hands_each_item_out_once() {
        let fetched = Arc::new(Mutex::new(Vec::new()));
        let log = fetched.clone();
        let ahead = Ahead::start(
            vec![
                ("a".into(), key("1"), "a"),
                ("b".into(), key("1"), "b"),
                ("a".into(), key("2"), "again"),
            ],
            1,
            move |job: &str| {
                log.lock().unwrap().push(job);
                (job != "b").then(|| job.to_owned())
            },
        );
        ahead.settle();
        ahead.stop(true);
        assert_eq!(*fetched.lock().unwrap(), ["a", "b"]);
        assert_eq!(ahead.take("a", &key("1")), Taken::Found("a".to_owned()));
        assert_eq!(ahead.take("a", &key("1")), Taken::Nothing);
        assert_eq!(ahead.take("b", &key("1")), Taken::Missing);
        assert_eq!(ahead.take("c", &key("1")), Taken::Nothing);
    }

    /// A task asking while its item waits in the queue fetches for itself,
    /// and the item is never fetched; one asking with another key does not
    /// wait for the fetch under way.
    #[test]
    fn a_queued_or_mismatched_item_is_not_waited_for() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let ahead = Ahead::start(
            vec![
                ("slow".into(), key("1"), "slow"),
                ("queued".into(), key("1"), "queued"),
            ],
            1,
            move |job: &str| {
                counted.fetch_add(1, Ordering::SeqCst);
                if job == "slow" {
                    released.lock().unwrap().recv().unwrap();
                }
                Some(job.to_owned())
            },
        );
        // The worker is held on the first item.
        while calls.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        assert_eq!(ahead.take("queued", &key("1")), Taken::Nothing);
        assert_eq!(ahead.take("slow", &key("2")), Taken::Nothing);
        release.send(()).unwrap();
        ahead.stop(true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A task asking while its item is fetched waits for that fetch.
    #[test]
    fn an_item_being_fetched_is_waited_for() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let started = Arc::new(AtomicUsize::new(0));
        let seen = started.clone();
        let ahead = Arc::new(Ahead::start(
            vec![("slow".into(), key("1"), "slow")],
            1,
            move |job: &str| {
                seen.fetch_add(1, Ordering::SeqCst);
                released.lock().unwrap().recv().unwrap();
                Some(job.to_owned())
            },
        ));
        while started.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let taker = {
            let ahead = ahead.clone();
            std::thread::spawn(move || ahead.take("slow", &key("1")))
        };
        release.send(()).unwrap();
        assert_eq!(taker.join().unwrap(), Taken::Found("slow".to_owned()));
        ahead.stop(true);
    }

    /// A fetch that panics found nothing, and a task waiting for it is let
    /// go; the worker goes on to the next item.
    #[test]
    fn a_fetch_that_panics_found_nothing() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let started = Arc::new(AtomicUsize::new(0));
        let seen = started.clone();
        let ahead = Arc::new(Ahead::start(
            vec![
                ("boom".into(), key("1"), "boom"),
                ("next".into(), key("1"), "next"),
            ],
            1,
            move |job: &str| {
                if job == "boom" {
                    seen.fetch_add(1, Ordering::SeqCst);
                    released.lock().unwrap().recv().unwrap();
                    panic!("fetch failed");
                }
                Some(job.to_owned())
            },
        ));
        while started.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let taker = {
            let ahead = ahead.clone();
            std::thread::spawn(move || ahead.take("boom", &key("1")))
        };
        release.send(()).unwrap();
        assert_eq!(taker.join().unwrap(), Taken::Missing);
        ahead.settle();
        assert_eq!(
            ahead.take("next", &key("1")),
            Taken::Found("next".to_owned())
        );
        ahead.stop(true);
    }

    /// Stopping drops the queue, so only what was under way is fetched.
    #[test]
    fn stopping_drops_what_is_still_queued() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let ahead: Ahead<&str, String> = Ahead::start(
            vec![
                ("first".into(), key("1"), "first"),
                ("second".into(), key("1"), "second"),
            ],
            1,
            move |job: &str| {
                counted.fetch_add(1, Ordering::SeqCst);
                released.lock().unwrap().recv().unwrap();
                Some(job.to_owned())
            },
        );
        while calls.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        // Not waiting, it returns while the fetch under way is held.
        ahead.stop(false);
        release.send(()).unwrap();
        // The worker, left to finish, stops before the next item.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
