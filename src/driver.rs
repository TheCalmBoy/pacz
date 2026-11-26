//! Runs aria2-next for a task and maps the outcome back to per-package results.
//!
//! Files are downloaded into a *staging* directory and moved into pacman's cache only
//! once aria2 reports them complete (and checksum-verified). A half-finished file can
//! therefore never sit in the cache under its final name, where the next
//! `pacman --print` would report it as "cached" and pacman would later find it corrupt.
use crate::aria2::{BatchProgress, ResultStatus};
use crate::conf::Repo;
use crate::plan::{aria2_input, Task};
use crate::rows::Pkg;
use crate::sched::{PkgResult, Runner};
use crate::urls::{resolve, Source};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

pub type ProgressFn = Box<dyn Fn(usize, &BatchProgress) + Send + Sync>;
static COUNTER: AtomicU64 = AtomicU64::new(0);
const EXDEV: i32 = 18;

pub struct Aria2Runner {
    /// Binary to run: "aria2-next" for your fork, "aria2c" for stock aria2.
    pub bin: String,
    pub cache_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub tmp_dir: PathBuf,
    pub repos: Vec<Repo>,
    pub check_integrity: bool,
    pub extra_args: Vec<String>,
    pub on_progress: Option<ProgressFn>,
    cancelled: AtomicBool,
    children: Mutex<Vec<u32>>,
}

/// Creates `path` with mode 0600; fails if it already exists (no symlink games in /tmp).
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    if let Err(e) = f.write_all(text.as_bytes()) {
        drop(f);
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

/// Rename, or copy + fsync + rename when staging and cache are on different filesystems.
fn move_into(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Err(e) if e.raw_os_error() == Some(EXDEV) => {
            let name = dst
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (tmp, mut output) = loop {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let path =
                    dst.with_file_name(format!(".{name}.pacz-tmp-{}-{n}", std::process::id()));
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                {
                    Ok(file) => break (path, file),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            };
            let res = (|| {
                let mut input = fs::File::open(src)?;
                let permissions = input.metadata()?.permissions();
                io::copy(&mut input, &mut output)?;
                output.set_permissions(permissions)?;
                output.sync_all()?;
                drop(output);
                fs::rename(&tmp, dst)?;
                fs::remove_file(src)
            })();
            if res.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            res
        }
        other => other,
    }
}

impl Aria2Runner {
    pub fn new(
        bin: impl Into<String>,
        cache_dir: PathBuf,
        staging_dir: PathBuf,
        tmp_dir: PathBuf,
        repos: Vec<Repo>,
    ) -> Self {
        Self {
            bin: bin.into(),
            cache_dir,
            staging_dir,
            state_dir: None,
            tmp_dir,
            repos,
            check_integrity: true,
            extra_args: vec![],
            on_progress: None,
            cancelled: AtomicBool::new(false),
            children: Mutex::new(vec![]),
        }
    }

    /// Start nothing new and terminate running aria2 processes. aria2-next stores resume
    /// data under state-dir; stock aria2 uses sidecars. The staged partials resume next run.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        for &pid in self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            crate::sys::terminate(pid);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn args(&self, batch: bool, input: &Path) -> Vec<String> {
        let mut a: Vec<String> = [
            "--continue",
            "--allow-overwrite=true",
            "--auto-file-renaming=false",
            "--file-allocation=none",
            "--summary-interval=1",
            "--console-log-level=notice",
            "--enable-color=false",
        ]
        .map(String::from)
        .into();
        if self.check_integrity {
            a.push("--check-integrity=true".into());
        }
        if batch {
            // Many small files: reuse connections, one stream per file.
            a.extend(
                [
                    "--max-concurrent-downloads=8",
                    "--split=1",
                    "--max-connection-per-server=1",
                ]
                .map(String::from),
            );
        } else {
            a.extend(
                [
                    "--max-concurrent-downloads=1",
                    "--split=8",
                    "--max-connection-per-server=8",
                    "--min-split-size=4M",
                ]
                .map(String::from),
            );
        }
        if let Some(s) = &self.state_dir {
            a.push(format!("--state-dir={}", s.display()));
        }
        a.extend(self.extra_args.iter().cloned());
        a.push("--input-file".into());
        a.push(input.display().to_string());
        a.push("--dir".into());
        a.push(self.staging_dir.display().to_string());
        a
    }

    /// Returns (progress, exited_ok).
    fn spawn(
        &self,
        id: usize,
        batch: bool,
        entries: &[(&Pkg, Vec<String>)],
    ) -> Result<(BatchProgress, bool), String> {
        let text = aria2_input(entries)?;
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let input = self
            .tmp_dir
            .join(format!("pacz-aria2-{}-{n}.txt", std::process::id()));
        write_private(&input, &text)
            .map_err(|e| format!("cannot write aria2 input {}: {e}", input.display()))?;
        let child = Command::new(&self.bin)
            .args(self.args(batch, &input))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = fs::remove_file(&input);
                return Err(format!("cannot start {}: {e}", self.bin));
            }
        };
        let pid = child.id();
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pid);
        if self.is_cancelled() {
            crate::sys::terminate(pid); // cancel() ran between our check and the spawn
        }
        let stderr = child.stderr.take().map(|mut e| {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = e.read_to_string(&mut s);
                s
            })
        });
        let mut prog = BatchProgress::new(entries.iter().map(|(p, _)| (p.file.clone(), p.size)));
        if let Some(out) = child.stdout.take() {
            // Byte-wise lines + lossy UTF-8: an odd byte must never stop us draining the
            // pipe (aria2 would block on a full pipe and we would wait forever).
            let mut r = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = String::from_utf8_lossy(&buf);
                for seg in text.split(['\r', '\n']).filter(|s| !s.is_empty()) {
                    prog.feed(seg);
                }
                if let Some(cb) = &self.on_progress {
                    cb(id, &prog);
                }
            }
        }
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|&p| p != pid);
        let err_text = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
        if prog.last_error.is_none() {
            prog.last_error = err_text
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(String::from);
        }
        let _ = fs::remove_file(&input);
        Ok((prog, ok))
    }

    fn settle(&self, p: &Pkg, prog: &BatchProgress, exited_ok: bool) -> Result<(), String> {
        let staged = self.staging_dir.join(&p.file);
        let control = self.staging_dir.join(format!("{}.aria2", p.file));
        // The results table is the most reliable signal; without it, fall back to
        // "whole process succeeded" or "seen completing".
        let status = prog.result_status(&p.file);
        let reported = prog
            .result(&p.file)
            .unwrap_or_else(|| exited_ok || prog.is_completed(&p.file));
        if reported && staged.is_file() && !control.exists() {
            return move_into(&staged, &self.cache_dir.join(&p.file))
                .map_err(|e| format!("cannot move into cache: {e}"));
        }
        // aria2-next reports INPR for interrupted transfers and can sparse-allocate the
        // full logical length while keeping resume ranges in state-dir. Keep those files.
        // Other full-length failures did not verify and are garbage; shorter files resume.
        if status != Some(ResultStatus::InProgress)
            && fs::metadata(&staged).is_ok_and(|m| p.size > 0 && m.len() >= p.size)
        {
            let _ = fs::remove_file(&staged);
            let _ = fs::remove_file(&control);
        }
        Err(if self.is_cancelled() {
            "interrupted".to_string()
        } else {
            prog.last_error.as_ref().map_or_else(
                || "download failed".to_string(),
                |e| format!("download failed: {e}"),
            )
        })
    }
}

impl Runner for Aria2Runner {
    fn run(&self, id: usize, task: &Task) -> Vec<PkgResult> {
        let batch = matches!(task, Task::Batch(_));
        if self.is_cancelled() {
            return task
                .pkgs()
                .iter()
                .map(|p| (p.name.clone(), Err("interrupted".to_string())))
                .collect();
        }
        let mut results: Vec<PkgResult> = Vec::new();
        let mut entries: Vec<(&Pkg, Vec<String>)> = Vec::new();
        for p in task.pkgs() {
            match resolve(&self.repos, p) {
                Source::Cached => results.push((p.name.clone(), Ok(()))),
                Source::None => results.push((p.name.clone(), Err("no usable mirror".into()))),
                Source::Urls(_) if p.sha256.is_none() => results.push((
                    p.name.clone(),
                    Err("no sha256, refusing unverified download".into()),
                )),
                Source::Urls(u) => entries.push((p, u)),
            }
        }
        if entries.is_empty() {
            return results;
        }
        match self.spawn(id, batch, &entries) {
            Err(e) => results.extend(
                entries
                    .iter()
                    .map(|(p, _)| (p.name.clone(), Err(e.clone()))),
            ),
            Ok((prog, ok)) => {
                for (p, _) in &entries {
                    results.push((p.name.clone(), self.settle(p, &prog, ok)));
                }
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::test_pkg;
    use crate::sched::run_all;
    use std::os::unix::fs::PermissionsExt;

    /// Stub aria2-next: writes every `out=` file into --dir and prints notices and a
    /// results table. FAIL*: not created. PART*: partial file + control file (resumable).
    /// BAD*: full-length garbage reported as ERR. Any of those makes it exit 1.
    const STUB: &str = r#"#!/bin/sh
echo "$@" >> @LOG@
while [ $# -gt 0 ]; do case "$1" in --input-file) IN="$2"; shift;; --dir) DIR="$2"; shift;; esac; shift; done
rc=0
echo "*** Download Progress Summary as of now ***"
echo "[#1 1B/9B(11%) CN:1 DL:2KiB ETA:1s]"
for out in $(grep '^  out=' "$IN" | sed 's/^  out=//'); do
  case "$out" in
    FAIL*) rc=1; echo "01/01 00:00:00 [ERROR] CUID#7 - Download aborted. URI=https://m/$out"; echo "abc123|ERR |   0B/s|https://m/$out";;
    PART*) rc=1; printf x > "$DIR/$out"; : > "$DIR/$out.aria2"; echo "abc123|INPR|   0B/s|$DIR/$out";;
    BAD*) rc=1; echo garbage-garbage-garbage > "$DIR/$out"; echo "abc123|ERR |   0B/s|$DIR/$out";;
    *) echo data > "$DIR/$out"; echo "01/01 00:00:00 [NOTICE] Download complete: $DIR/$out"; echo "abc123|OK  |   1KiB/s|$DIR/$out";;
  esac
done
exit $rc
"#;

    fn setup(tag: &str) -> (Aria2Runner, PathBuf) {
        let root = std::env::temp_dir().join(format!("pacz-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in ["cache", "staging"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let bin = root.join("aria2-next");
        fs::write(
            &bin,
            STUB.replace(
                "@LOG@",
                &format!("\"{}\"", root.join("calls.log").display()),
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let r = Aria2Runner::new(
            bin.display().to_string(),
            root.join("cache"),
            root.join("staging"),
            root.clone(),
            vec![],
        );
        (r, root)
    }

    fn count(dir: PathBuf) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    fn quiet(_: usize, _: &[PkgResult]) {}

    #[test]
    fn batch_runs_one_process_and_files_land_in_cache_not_staging() {
        let (r, root) = setup("batch");
        let pkgs: Vec<Pkg> = (0..40).map(|i| test_pkg(&format!("s{i}"), 1000)).collect();
        let res = run_all(&[Task::Batch(pkgs)], 4, &r, &quiet);
        assert_eq!(res.len(), 40);
        assert!(res.iter().all(|(_, r)| r.is_ok()), "{res:?}");
        assert_eq!(count(root.join("cache")), 40);
        assert_eq!(count(root.join("staging")), 0);
        let log = fs::read_to_string(root.join("calls.log")).unwrap();
        assert_eq!(
            log.lines().count(),
            1,
            "one aria2 process for the whole batch"
        );
        assert!(log.contains("--max-concurrent-downloads=8") && log.contains("--split=1"));
        assert!(log.contains("--check-integrity=true"));
        assert!(log.contains(&format!("--dir {}", root.join("staging").display())));
        assert!(
            !root.read_dir().unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("pacz-aria2-")),
            "input file removed"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn partial_failure_marks_only_the_failed_file_with_aria2_reason() {
        let (r, root) = setup("fail");
        let res = run_all(
            &[Task::Batch(vec![
                test_pkg("good1", 10),
                test_pkg("FAIL1", 10),
                test_pkg("good2", 10),
            ])],
            1,
            &r,
            &quiet,
        );
        let bad: Vec<_> = res
            .iter()
            .filter(|(_, r)| r.is_err())
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(bad, ["FAIL1"]);
        let why = res
            .iter()
            .find(|(n, _)| n == "FAIL1")
            .unwrap()
            .1
            .clone()
            .unwrap_err();
        assert!(why.contains("Download aborted"), "{why}");
        assert!(
            root.join("cache/good1.pkg.tar.zst").exists()
                && !root.join("cache/FAIL1.pkg.tar.zst").exists()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn partial_download_kept_for_resume_and_bad_full_file_discarded() {
        let (r, root) = setup("partial");
        let res = run_all(
            &[Task::Batch(vec![
                test_pkg("PART1", 1000),
                test_pkg("BAD1", 10),
                test_pkg("ok1", 10),
            ])],
            1,
            &r,
            &quiet,
        );
        let bad: Vec<_> = res
            .iter()
            .filter(|(_, r)| r.is_err())
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(bad, ["PART1", "BAD1"]);
        assert!(
            root.join("staging/PART1.pkg.tar.zst").exists(),
            "partial kept in staging"
        );
        assert!(
            !root.join("staging/BAD1.pkg.tar.zst").exists(),
            "full-length bad file removed"
        );
        assert!(
            !root.join("cache/PART1.pkg.tar.zst").exists()
                && !root.join("cache/BAD1.pkg.tar.zst").exists()
        );
        assert!(root.join("cache/ok1.pkg.tar.zst").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn state_dir_in_progress_result_keeps_sparse_full_length_partial() {
        let (mut r, root) = setup("state-inpr");
        let pkg = Pkg {
            name: "sample".into(),
            version: "1-1".into(),
            size: 134_217_728,
            file: "sample.pkg.tar.zst".into(),
            repo: "extra".into(),
            url: "https://example.invalid/sample.pkg".into(),
            sha256: Some("a".repeat(64)),
        };
        r.state_dir = Some(root.join("state"));
        fs::create_dir_all(r.state_dir.as_ref().unwrap()).unwrap();
        let staged = r.staging_dir.join(&pkg.file);
        fs::File::create(&staged)
            .unwrap()
            .set_len(pkg.size)
            .unwrap();
        let mut progress = BatchProgress::new([(pkg.file.clone(), pkg.size)]);
        for line in include_str!("../tests/fixtures/aria2-next-interrupt.txt").lines() {
            progress.feed(line);
        }

        assert_eq!(
            progress.result_status(&pkg.file),
            Some(ResultStatus::InProgress)
        );
        assert!(r.settle(&pkg, &progress, false).is_err());
        assert!(
            staged.exists(),
            "aria2-next resume metadata uses state-dir, not a sidecar next to this file"
        );
        assert!(!r.cache_dir.join(&pkg.file).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn single_uses_split_flags_cached_skipped_and_sha_required() {
        let (r, root) = setup("single");
        let mut cached = test_pkg("c", 5);
        cached.url = "file:///x".into();
        let mut nosha = test_pkg("n", 5);
        nosha.sha256 = None;
        let tasks = [
            Task::Single(test_pkg("big", 50 << 20)),
            Task::Single(cached),
            Task::Single(nosha),
        ];
        let res = run_all(&tasks, 2, &r, &quiet);
        let ok: Vec<_> = res
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(ok.contains(&"big") && ok.contains(&"c") && !ok.contains(&"n"));
        let log = fs::read_to_string(root.join("calls.log")).unwrap();
        assert_eq!(log.lines().count(), 1);
        assert!(log.contains("--split=8") && log.contains("--min-split-size=4M"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_binary_fails_cleanly() {
        let (mut r, root) = setup("nobin");
        r.bin = "/nonexistent/aria2-next".into();
        let res = run_all(&[Task::Single(test_pkg("a", 1))], 1, &r, &quiet);
        assert!(res[0].1.as_ref().unwrap_err().contains("cannot start"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_runner_starts_nothing() {
        let (r, root) = setup("cancel");
        r.cancel();
        let res = run_all(
            &[
                Task::Single(test_pkg("a", 1)),
                Task::Batch(vec![test_pkg("b", 1), test_pkg("c", 1)]),
            ],
            2,
            &r,
            &quiet,
        );
        assert_eq!(res.len(), 3);
        assert!(res
            .iter()
            .all(|(_, r)| r.as_ref().unwrap_err() == "interrupted"));
        assert!(!root.join("calls.log").exists());
        let _ = fs::remove_dir_all(root);
    }
}
