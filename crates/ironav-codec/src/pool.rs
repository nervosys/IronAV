//! A few worker threads kept alive between calls, for the live H.264 encoder: it shares each
//! picture's work among threads a dozen times over, and starting a thread for each share costs a
//! good part of a frame time on some systems.

use std::any::Any;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

/// The threads the machine runs at once.
pub fn threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One call of `run`: `n` shares of work, taken in turn by whichever thread is free.
struct Job {
    /// The caller's closure, with its lifetime erased: `run` does not return until every share that
    /// started is done, and no thread calls it for a share it has not started.
    f: *const (dyn Fn(usize) + Sync),
    n: usize,
    next: AtomicUsize,
    done: AtomicUsize,
    finished: Mutex<bool>,
    finish: Condvar,
    panic: Mutex<Option<Box<dyn Any + Send>>>,
}

// SAFETY: `f` is only called through a shared reference to a `Sync` closure, and see above.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    /// Takes shares until none are left.
    fn work(&self) {
        loop {
            let i = self.next.fetch_add(1, Ordering::AcqRel);
            if i >= self.n {
                return;
            }
            // SAFETY: share `i` started, so `run` is still waiting for it (see `f`).
            let f = unsafe { &*self.f };
            if let Err(p) = catch_unwind(AssertUnwindSafe(|| f(i))) {
                lock(&self.panic).get_or_insert(p);
            }
            if self.done.fetch_add(1, Ordering::AcqRel) + 1 == self.n {
                *lock(&self.finished) = true;
                self.finish.notify_all();
            }
        }
    }
}

struct Pool {
    /// A count of jobs posted, and the latest.
    state: Mutex<(u64, Option<Arc<Job>>)>,
    wake: Condvar,
    /// Held while a job runs: a second caller meanwhile uses threads of its own.
    busy: Mutex<()>,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool: &'static Pool = Box::leak(Box::new(Pool { state: Mutex::new((0, None)), wake: Condvar::new(), busy: Mutex::new(()) }));
        for _ in 1..threads() {
            let spawned = std::thread::Builder::new().name("ironav-pool".into()).spawn(move || {
                let mut seen = 0;
                loop {
                    let job = {
                        let mut st = lock(&pool.state);
                        while st.0 == seen {
                            st = pool.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
                        }
                        seen = st.0;
                        st.1.clone()
                    };
                    if let Some(job) = job {
                        job.work();
                    }
                }
            });
            if spawned.is_err() {
                break;
            }
        }
        pool
    })
}

/// Calls `f(0)` to `f(n - 1)`, on up to `threads()` threads at once, and returns when all are done.
/// A panic in `f` is raised here once the others have finished.
pub fn run(n: usize, f: impl Fn(usize) + Sync) {
    if n <= 1 {
        (0..n).for_each(f);
        return;
    }
    let pool = pool();
    let Ok(_busy) = pool.busy.try_lock() else {
        // another thread is using the pool: start threads for this call
        std::thread::scope(|s| {
            for i in 0..n {
                let f = &f;
                s.spawn(move || f(i));
            }
        });
        return;
    };
    let fr: &(dyn Fn(usize) + Sync) = &f;
    // SAFETY: see `Job::f`; the job is dropped from the pool and waited for below.
    let fp: *const (dyn Fn(usize) + Sync) = unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(fr) };
    let job = Arc::new(Job { f: fp, n, next: AtomicUsize::new(0), done: AtomicUsize::new(0), finished: Mutex::new(false), finish: Condvar::new(), panic: Mutex::new(None) });
    {
        let mut st = lock(&pool.state);
        st.0 += 1;
        st.1 = Some(job.clone());
    }
    pool.wake.notify_all();
    job.work();
    {
        let mut fin = lock(&job.finished);
        while !*fin {
            fin = job.finish.wait(fin).unwrap_or_else(PoisonError::into_inner);
        }
    }
    lock(&pool.state).1 = None;
    let panicked = lock(&job.panic).take();
    if let Some(p) = panicked {
        resume_unwind(p);
    }
}

/// `f(0)` to `f(n - 1)` run by `run`, in a vector.
pub fn map<R: Send>(n: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let slots: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    run(n, |i| *lock(&slots[i]) = Some(f(i)));
    slots.into_iter().map(|s| s.into_inner().unwrap_or_else(PoisonError::into_inner).expect("a share did not run")).collect()
}

/// `f(i, item)` for each item (mutable slices, say) in turn by index, as `run`.
pub fn each<T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync) {
    let n = items.len();
    let slots: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    run(n, |i| {
        if let Some(t) = lock(&slots[i]).take() {
            f(i, t);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_share_runs_once_and_in_the_result_order() {
        for n in [0, 1, 2, 7, 64] {
            let out = map(n, |i| i * i);
            assert_eq!(out, (0..n).map(|i| i * i).collect::<Vec<_>>());
        }
        let hits = AtomicUsize::new(0);
        run(100, |_| {
            hits.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(hits.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn a_panic_in_a_share_is_raised_in_the_caller_and_the_pool_survives() {
        let r = catch_unwind(|| run(8, |i| assert_ne!(i, 5)));
        assert!(r.is_err());
        assert_eq!(map(4, |i| i), vec![0, 1, 2, 3]);
    }

    #[test]
    fn callers_on_several_threads_at_once_all_finish() {
        let hs: Vec<_> = (0..4).map(|k| std::thread::spawn(move || (0..50).map(|r| map(9, |i| i + k + r).iter().sum::<usize>()).sum::<usize>())).collect();
        for (k, h) in hs.into_iter().enumerate() {
            let want: usize = (0..50).map(|r| (0..9).map(|i| i + k + r).sum::<usize>()).sum();
            assert_eq!(h.join().unwrap(), want);
        }
    }
}
