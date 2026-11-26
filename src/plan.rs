//! Download planning: batch many small packages into few aria2 processes,
//! keep big ones individual, run biggest first.
use crate::rows::Pkg;

#[derive(Debug, Clone)]
pub struct PlanCfg {
    /// Packages smaller than this are "small".
    pub small_bytes: u64,
    /// Max files per batch.
    pub batch_max: usize,
    /// Don't bother batching fewer small packages than this.
    pub batch_min: usize,
}

impl Default for PlanCfg {
    fn default() -> Self {
        Self { small_bytes: 8 << 20, batch_max: 40, batch_min: 3 }
    }
}

#[derive(Debug, Clone)]
pub enum Task {
    Single(Pkg),
    Batch(Vec<Pkg>),
}

impl Task {
    pub fn bytes(&self) -> u64 {
        self.pkgs().iter().map(|p| p.size).sum()
    }

    pub fn pkgs(&self) -> &[Pkg] {
        match self {
            Task::Single(p) => std::slice::from_ref(p),
            Task::Batch(v) => v.as_slice(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Ordered: singles by size descending (long poles first), then batches.
    pub tasks: Vec<Task>,
    /// Already in pacman's cache.
    pub cached: Vec<Pkg>,
    /// No sha256 in the sync DB: pacz will not download these (pacman may, see config).
    pub no_sha: Vec<Pkg>,
}

pub fn plan(pkgs: Vec<Pkg>, cfg: &PlanCfg) -> Plan {
    let (cached, todo): (Vec<_>, Vec<_>) = pkgs.into_iter().partition(Pkg::is_cached);
    let (todo, no_sha): (Vec<Pkg>, Vec<Pkg>) = todo.into_iter().partition(|p| p.sha256.is_some());
    let (mut small, big): (Vec<_>, Vec<_>) = todo.into_iter().partition(|p| p.size < cfg.small_bytes);
    let batch_max = cfg.batch_max.max(1);

    let mut singles: Vec<Pkg> = big;
    let mut batches: Vec<Vec<Pkg>> = Vec::new();
    if small.len() >= cfg.batch_min.max(1) {
        let n = small.len().div_ceil(batch_max);
        batches = vec![Vec::new(); n];
        let mut load = vec![0u64; n];
        small.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        for p in small {
            // Least-loaded batch that still has room: balances bytes across batches.
            let i = (0..n)
                .filter(|&i| batches[i].len() < batch_max)
                .min_by_key(|&i| load[i])
                .expect("capacity = n * batch_max >= small.len()");
            load[i] += p.size;
            batches[i].push(p);
        }
    } else {
        singles.extend(small);
    }
    singles.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    batches.sort_by_key(|b| std::cmp::Reverse(b.iter().map(|p| p.size).sum::<u64>()));

    let mut tasks: Vec<Task> = singles.into_iter().map(Task::Single).collect();
    tasks.extend(batches.into_iter().map(Task::Batch));
    Plan { tasks, cached, no_sha }
}

/// One unique display label per task: the package name, or "batch k/N (m pkgs)".
pub fn labels(tasks: &[Task]) -> Vec<String> {
    let nb = tasks.iter().filter(|t| matches!(t, Task::Batch(_))).count();
    let mut k = 0;
    tasks
        .iter()
        .map(|t| match t {
            Task::Single(p) => p.name.clone(),
            Task::Batch(v) => {
                k += 1;
                format!("batch {k}/{nb} ({} pkgs)", v.len())
            }
        })
        .collect()
}

/// aria2 `--input-file` content: one line per file (tab-separated mirrors),
/// followed by indented per-file options. Packages without a sha256 are refused.
pub fn aria2_input(entries: &[(&Pkg, Vec<String>)]) -> Result<String, String> {
    let mut out = String::new();
    for (p, urls) in entries {
        let sha = p.sha256.as_deref().ok_or_else(|| format!("{}: no sha256, refusing unverified download", p.name))?;
        if urls.is_empty() {
            return Err(format!("{}: no mirrors", p.name));
        }
        out.push_str(&urls.join("\t"));
        out.push_str(&format!("\n  checksum=sha-256={sha}\n  out={}\n", p.file));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::test_pkg;

    fn pkg(name: &str, size: u64, cached: bool) -> Pkg {
        let mut p = test_pkg(name, size);
        if cached {
            p.url = "file:///c".into();
        }
        p
    }

    #[test]
    fn hundred_fifty_tiny_plus_three_big() {
        let mut v: Vec<Pkg> = (0..150).map(|i| pkg(&format!("s{i}"), 1_000 + i as u64 * 300, false)).collect();
        v.extend((0..3).map(|i| pkg(&format!("big{i}"), (100 + i as u64) << 20, false)));
        let p = plan(v, &PlanCfg::default());
        assert!(p.cached.is_empty() && p.no_sha.is_empty());
        // 3 singles first (largest first), then ceil(150/40) = 4 batches
        assert_eq!(p.tasks.len(), 3 + 4);
        assert!(matches!(&p.tasks[0], Task::Single(x) if x.name == "big2"));
        let batches: Vec<_> = p.tasks.iter().filter_map(|t| if let Task::Batch(b) = t { Some(b) } else { None }).collect();
        assert_eq!(batches.len(), 4);
        assert!(batches.iter().all(|b| b.len() <= 40));
        let sizes: Vec<u64> = batches.iter().map(|b| b.iter().map(|p| p.size).sum()).collect();
        let (lo, hi) = (sizes.iter().min().unwrap(), sizes.iter().max().unwrap());
        assert!(hi - lo < 10_000, "batches should be byte-balanced: {sizes:?}");
        let mut names: Vec<String> = p.tasks.iter().flat_map(|t| t.pkgs().iter().map(|p| p.name.clone())).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 153);
        // Labels must be unique even when batches have equal sizes (37/37/38/38 here).
        let mut l = labels(&p.tasks);
        l.sort();
        l.dedup();
        assert_eq!(l.len(), 7);
    }

    #[test]
    fn few_small_packages_stay_single_cached_and_unverified_are_split_off() {
        let mut nosha = pkg("n", 5, false);
        nosha.sha256 = None;
        let v = vec![pkg("a", 10, false), pkg("b", 20, false), pkg("c", 30, true), nosha];
        let p = plan(v, &PlanCfg::default());
        assert_eq!(p.cached.len(), 1);
        assert_eq!(p.no_sha.len(), 1);
        assert_eq!(p.tasks.len(), 2);
        assert!(p.tasks.iter().all(|t| matches!(t, Task::Single(_))));
    }

    #[test]
    fn zero_batch_max_does_not_panic() {
        let v: Vec<Pkg> = (0..5).map(|i| pkg(&format!("s{i}"), 10, false)).collect();
        let p = plan(v, &PlanCfg { small_bytes: 100, batch_max: 0, batch_min: 0 });
        assert_eq!(p.tasks.iter().map(|t| t.pkgs().len()).sum::<usize>(), 5);
    }

    #[test]
    fn aria2_input_format_and_sha_enforcement() {
        let p = pkg("a", 10, false);
        let s = aria2_input(&[(&p, vec!["https://x/a".into(), "https://y/a".into()])]).unwrap();
        assert_eq!(s, format!("https://x/a\thttps://y/a\n  checksum=sha-256={}\n  out=a.pkg.tar.zst\n", "a".repeat(64)));
        let mut q = pkg("q", 10, false);
        q.sha256 = None;
        assert!(aria2_input(&[(&q, vec!["https://x/q".into()])]).is_err());
    }
}
