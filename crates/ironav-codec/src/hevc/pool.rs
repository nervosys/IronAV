//! A small pool of worker threads for the loop filters. Spawning threads for every picture costs about
//! as much as the filters save, so the workers stay alive between pictures.

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub(super) struct Pool {
    tx: Option<mpsc::Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

/// A job that borrows from the caller; `Pool::run` returns only after it has finished.
pub(super) type ScopedJob<'a> = Box<dyn FnOnce() + Send + 'a>;

impl Pool {
    /// A pool of `n` worker threads (the calling thread also runs jobs, so `n + 1` run at once).
    pub(super) fn new(n: usize) -> Pool {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..n)
            .map(|_| {
                let rx = rx.clone();
                std::thread::spawn(move || loop {
                    let job = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    match job {
                        Ok(job) => job(),
                        Err(_) => break,
                    }
                })
            })
            .collect();
        Pool { tx: Some(tx), workers }
    }

    /// How many jobs run at once.
    pub(super) fn width(&self) -> usize {
        self.workers.len() + 1
    }

    /// Runs the jobs, the first on the calling thread, and returns when all have finished. A panic in
    /// any of them is raised here once the others are done.
    pub(super) fn run(&self, mut jobs: Vec<ScopedJob<'_>>) {
        if jobs.is_empty() {
            return;
        }
        let first = jobs.remove(0);
        let pending = Arc::new((Mutex::new(jobs.len()), Condvar::new()));
        let failed = Arc::new(AtomicBool::new(false));
        for job in jobs {
            // SAFETY: this function does not return before every job sent below has finished (it waits
            // on `pending`, even if the first job panics), so what the job borrows outlives its run.
            let job: Job = unsafe { std::mem::transmute::<ScopedJob<'_>, Job>(job) };
            let (pending, failed) = (pending.clone(), failed.clone());
            let wrapped: Job = Box::new(move || {
                if catch_unwind(AssertUnwindSafe(job)).is_err() {
                    failed.store(true, Ordering::SeqCst);
                }
                let (count, done) = &*pending;
                *count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                done.notify_all();
            });
            self.tx.as_ref().expect("the pool is running").send(wrapped).expect("a worker thread is alive");
        }
        let mine = catch_unwind(AssertUnwindSafe(first));
        let (count, done) = &*pending;
        let mut left = count.lock().unwrap_or_else(|e| e.into_inner());
        while *left > 0 {
            left = done.wait(left).unwrap_or_else(|e| e.into_inner());
        }
        drop(left);
        if let Err(e) = mine {
            resume_unwind(e);
        }
        assert!(!failed.load(Ordering::SeqCst), "a loop filter job panicked");
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.tx = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_run_to_completion_and_may_borrow() {
        let pool = Pool::new(3);
        let mut out = [0u32; 10];
        for round in 1..50u32 {
            let jobs: Vec<ScopedJob> = out
                .chunks_mut(3)
                .enumerate()
                .map(|(k, c)| -> ScopedJob { Box::new(move || c.iter_mut().for_each(|v| *v = round * 10 + k as u32)) })
                .collect();
            pool.run(jobs);
            assert_eq!(out[0], round * 10);
            assert_eq!(out[9], round * 10 + 3);
        }
    }

    #[test]
    #[should_panic(expected = "panicked")]
    fn a_panicking_job_is_reported() {
        let pool = Pool::new(2);
        let jobs: Vec<ScopedJob> = vec![Box::new(|| {}), Box::new(|| panic!("boom")), Box::new(|| {})];
        pool.run(jobs);
    }
}
