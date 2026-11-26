//! Worker pool: a free worker immediately takes the next task (no polling delay),
//! a failing or panicking task never affects the others.
use crate::plan::Task;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub type PkgResult = (String, Result<(), String>);

pub trait Runner: Sync {
    /// Runs task number `id`; returns a result per package name in it.
    fn run(&self, id: usize, task: &Task) -> Vec<PkgResult>;
}

/// Tasks start in the given order. `on_done(id, results)` is called from the worker
/// thread as soon as a task finishes. Returns one outcome per package.
pub fn run_all(
    tasks: &[Task],
    workers: usize,
    runner: &dyn Runner,
    on_done: &(dyn Fn(usize, &[PkgResult]) + Sync),
) -> Vec<PkgResult> {
    let next = AtomicUsize::new(0);
    let out = Mutex::new(Vec::new());
    let workers = workers.clamp(1, tasks.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let id = next.fetch_add(1, Ordering::SeqCst);
                let Some(task) = tasks.get(id) else { break };
                let res = catch_unwind(AssertUnwindSafe(|| runner.run(id, task))).unwrap_or_else(|_| {
                    task.pkgs().iter().map(|p| (p.name.clone(), Err("download task panicked".to_string()))).collect()
                });
                let _ = catch_unwind(AssertUnwindSafe(|| on_done(id, &res)));
                out.lock().unwrap_or_else(|e| e.into_inner()).extend(res);
            });
        }
    });
    out.into_inner().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::test_pkg;
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Duration;

    fn task(name: &str) -> Task {
        Task::Single(test_pkg(name, 1))
    }

    struct Fake {
        now: AtomicUsize,
        peak: AtomicUsize,
        order: Mutex<Vec<String>>,
    }
    impl Runner for Fake {
        fn run(&self, _id: usize, t: &Task) -> Vec<PkgResult> {
            let n = t.pkgs()[0].name.clone();
            self.order.lock().unwrap().push(n.clone());
            let c = self.now.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(c, SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            self.now.fetch_sub(1, SeqCst);
            if n == "bad" {
                panic!("boom")
            }
            let r = if n == "fail" { Err("nope".to_string()) } else { Ok(()) };
            vec![(n, r)]
        }
    }

    #[test]
    fn caps_concurrency_isolates_failures_and_keeps_order() {
        let f = Fake { now: 0.into(), peak: 0.into(), order: Mutex::new(vec![]) };
        let mut ts: Vec<Task> = (0..12).map(|i| task(&format!("p{i}"))).collect();
        ts.insert(1, task("fail"));
        ts.insert(2, task("bad"));
        let done = AtomicUsize::new(0);
        let res = run_all(&ts, 3, &f, &|_: usize, r: &[PkgResult]| {
            done.fetch_add(r.len(), SeqCst);
        });
        assert_eq!(res.len(), 14);
        assert_eq!(done.load(SeqCst), 14, "on_done sees every task, including the panicking one");
        assert!(f.peak.load(SeqCst) <= 3 && f.peak.load(SeqCst) >= 2);
        assert_eq!(res.iter().filter(|(_, r)| r.is_err()).count(), 2);
        let order = f.order.lock().unwrap();
        assert_eq!(
            &order[..3].iter().cloned().collect::<std::collections::HashSet<_>>(),
            &["p0", "fail", "bad"].iter().map(|s| s.to_string()).collect()
        );
    }
}
