//! Parsing aria2's stdout: progress readouts, summary blocks, completion notices,
//! errors and the final "Download Results" table.
//!
//! Progress is tracked per download (gid) and *replaced* on every update, so the same
//! download appearing in both a summary block and a readout line is never counted twice.
use crate::fmt::{parse_size, strip_ansi};
use std::collections::{HashMap, HashSet};

/// One `#gid ...` progress entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prog {
    pub gid: String,
    pub done: u64,
    pub total: u64,
    pub conns: Option<u32>,
    pub speed: Option<u64>,
    pub eta: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileProgress {
    pub done: u64,
    pub total: u64,
    pub speed: u64,
    pub conns: u32,
    pub eta: Option<u64>,
    pub active: bool,
    pub completed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// `[#gid 1.0MiB/10MiB(10%) CN:1 DL:1.0MiB ETA:9s]` or the compact multi-download
    /// readout `[DL:2MiB][#a 1MiB/2MiB(50%)][#b ...]`; `speed` is the global `DL:` value.
    Progress {
        items: Vec<Prog>,
        speed: Option<u64>,
    },
    /// `FILE: /path` under a summary entry (names the gid just above it).
    File(String),
    /// `[NOTICE] Download complete: /path/file`
    Complete(String),
    /// `*** Download Progress Summary as of ... ***` starts a new block.
    BlockStart,
    /// A row of the final results table: `gid|OK  |  1.2MiB/s|/path`.
    Result { status: ResultStatus, path: String },
    /// `[ERROR] ...` or `errorCode=N ...`
    Error(String),
}

/// Status from aria2's final results table. `InProgress` can have a sparse file whose
/// logical length already equals the final package size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    Ok,
    Error,
    InProgress,
    Removed,
}

fn parse_eta(s: &str) -> Option<u64> {
    let (mut total, mut num, mut any) = (0u64, String::new(), false);
    for c in s.trim_end_matches(']').chars() {
        match c {
            '0'..='9' => num.push(c),
            'd' | 'h' | 'm' | 's' => {
                let n: u64 = num.parse().ok()?;
                total += n * match c {
                    'd' => 86400,
                    'h' => 3600,
                    'm' => 60,
                    _ => 1,
                };
                num.clear();
                any = true;
            }
            _ => return None,
        }
    }
    any.then_some(total)
}

fn parse_group(g: &str) -> Option<Prog> {
    let mut toks = g.split_whitespace();
    let gid = toks.next()?.strip_prefix('#')?.to_owned();
    let mut p = Prog {
        gid,
        ..Default::default()
    };
    for tok in toks {
        if let Some(v) = tok.strip_prefix("CN:") {
            p.conns = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("DL:") {
            p.speed = parse_size(v);
        } else if let Some(v) = tok.strip_prefix("ETA:") {
            p.eta = parse_eta(v);
        } else if let Some((d, rest)) = tok.split_once('/') {
            p.done = parse_size(d).unwrap_or(0);
            p.total = parse_size(rest.split('(').next().unwrap_or("")).unwrap_or(0);
        }
    }
    Some(p)
}

fn parse_result_row(line: &str) -> Option<Event> {
    let f: Vec<&str> = line.split('|').collect();
    if f.len() != 4 {
        return None;
    }
    let gid = f[0].trim();
    if gid.is_empty() || !gid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let status = match f[1].trim() {
        "OK" => ResultStatus::Ok,
        "ERR" => ResultStatus::Error,
        "INPR" => ResultStatus::InProgress,
        "RM" => ResultStatus::Removed,
        _ => return None,
    };
    Some(Event::Result {
        status,
        path: f[3].trim().to_owned(),
    })
}

pub fn parse_line(raw: &str) -> Option<Event> {
    let clean = strip_ansi(raw);
    let line = clean.trim();
    if line.is_empty() {
        return None;
    }
    if line.contains("Download Progress Summary") {
        return Some(Event::BlockStart);
    }
    if let Some(i) = line.find("Download complete: ") {
        return Some(Event::Complete(
            line[i + "Download complete: ".len()..].trim().to_owned(),
        ));
    }
    if let Some(path) = line.strip_prefix("FILE: ") {
        return Some(Event::File(path.trim().to_owned()));
    }
    if let Some(ev) = parse_result_row(line) {
        return Some(ev);
    }
    if let Some(i) = line.find("[ERROR]") {
        return Some(Event::Error(line[i + "[ERROR]".len()..].trim().to_owned()));
    }
    if let Some(i) = line.rfind("errorCode=") {
        return Some(Event::Error(line[i..].trim().to_owned()));
    }
    if line.starts_with("[#") || line.starts_with("[DL:") {
        let (mut items, mut speed) = (Vec::new(), None);
        for g in line.split('[').filter_map(|s| s.split(']').next()) {
            if let Some(v) = g.strip_prefix("DL:") {
                speed = parse_size(v);
            } else if g.starts_with('#') {
                items.extend(parse_group(g));
            }
        }
        if items.is_empty() && speed.is_none() {
            return None;
        }
        return Some(Event::Progress { items, speed });
    }
    None
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Aggregates the output of one aria2 process (a single file or a batch).
#[derive(Debug, Default)]
pub struct BatchProgress {
    sizes: HashMap<String, u64>,
    completed: HashSet<String>,
    results: HashMap<String, ResultStatus>,
    /// Latest progress of each running download.
    live: HashMap<String, Prog>,
    gid_file: HashMap<String, String>,
    last_gid: Option<String>,
    global_speed: Option<u64>,
    done_bytes: u64,
    /// The most recent error aria2 printed, for the failure message.
    pub last_error: Option<String>,
}

impl BatchProgress {
    /// `members`: (file name, size in bytes) for every file in the batch.
    pub fn new(members: impl IntoIterator<Item = (String, u64)>) -> Self {
        Self {
            sizes: members.into_iter().collect(),
            ..Default::default()
        }
    }

    pub fn feed(&mut self, line: &str) {
        match parse_line(line) {
            Some(Event::BlockStart) => {
                // A summary block lists every running download again, with speeds.
                self.live.clear();
                self.global_speed = None;
                self.last_gid = None;
            }
            Some(Event::Progress { items, speed }) => {
                if speed.is_some() {
                    self.global_speed = speed;
                }
                for p in items {
                    if self
                        .gid_file
                        .get(&p.gid)
                        .is_some_and(|f| self.completed.contains(f))
                    {
                        continue; // late readout of a file that already finished
                    }
                    self.last_gid = Some(p.gid.clone());
                    let e = self.live.entry(p.gid.clone()).or_default();
                    e.gid = p.gid;
                    e.done = p.done;
                    e.total = p.total;
                    if p.conns.is_some() {
                        e.conns = p.conns;
                    }
                    if p.speed.is_some() {
                        e.speed = p.speed;
                    }
                    if p.eta.is_some() {
                        e.eta = p.eta;
                    }
                }
            }
            Some(Event::File(path)) => {
                if let Some(g) = self.last_gid.take() {
                    self.gid_file.insert(g, base(&path).to_owned());
                }
            }
            Some(Event::Complete(path)) => {
                let b = base(&path).to_owned();
                if let Some(sz) = self.sizes.get(&b).copied() {
                    if self.completed.insert(b.clone()) {
                        self.done_bytes += sz;
                    }
                    if self.sizes.len() == 1 {
                        self.live.clear();
                    } else {
                        self.live.retain(|g, _| self.gid_file.get(g) != Some(&b));
                    }
                }
            }
            Some(Event::Result { status, path }) => {
                self.results.insert(base(&path).to_owned(), status);
            }
            Some(Event::Error(msg)) => self.last_error = Some(msg),
            None => {}
        }
    }

    pub fn total(&self) -> u64 {
        self.sizes.values().sum()
    }

    /// Bytes of finished files plus progress of running ones (never above `total`).
    pub fn done(&self) -> u64 {
        (self.done_bytes + self.live.values().map(|p| p.done).sum::<u64>()).min(self.total())
    }

    pub fn speed(&self) -> u64 {
        self.global_speed
            .unwrap_or_else(|| self.live.values().filter_map(|p| p.speed).sum())
    }

    /// Open connections (falls back to 1 per running download when aria2 did not say).
    pub fn conns(&self) -> u32 {
        self.live.values().map(|p| p.conns.unwrap_or(1)).sum()
    }

    pub fn active(&self) -> usize {
        self.live.len()
    }

    pub fn percent(&self) -> u8 {
        let percent = (u128::from(self.done()) * 100)
            .checked_div(u128::from(self.total()))
            .unwrap_or(100);
        percent.min(100) as u8
    }

    pub fn eta(&self) -> Option<u64> {
        let s = self.speed();
        (s > 0).then(|| (self.total() - self.done()).div_ceil(s))
    }

    pub fn is_completed(&self, file: &str) -> bool {
        self.completed.contains(file)
    }

    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    /// Progress for one package in a multi-file aria2 task, if aria2 has identified it.
    pub fn file_progress(&self, file: &str) -> Option<FileProgress> {
        let total = *self.sizes.get(file)?;
        let completed =
            self.completed.contains(file) || self.result_status(file) == Some(ResultStatus::Ok);
        if completed {
            return Some(FileProgress {
                done: total,
                total,
                speed: 0,
                conns: 0,
                eta: Some(0),
                active: false,
                completed: true,
            });
        }

        let mut active = false;
        let mut done = 0u64;
        let mut speed = 0u64;
        let mut conns = 0u32;
        for (gid, progress) in &self.live {
            if self.gid_file.get(gid).map(String::as_str) != Some(file) {
                continue;
            }
            active = true;
            done = done.saturating_add(progress.done);
            speed = speed.saturating_add(progress.speed.unwrap_or(0));
            conns = conns.saturating_add(progress.conns.unwrap_or(1));
        }
        if !active {
            return None;
        }
        let done = done.min(total);
        let eta = (speed > 0).then(|| total.saturating_sub(done).div_ceil(speed));
        Some(FileProgress {
            done,
            total,
            speed,
            conns,
            eta,
            active,
            completed: false,
        })
    }

    /// What the final results table said about this file, if it was printed.
    pub fn result(&self, file: &str) -> Option<bool> {
        self.result_status(file)
            .map(|status| status == ResultStatus::Ok)
    }

    pub fn result_status(&self, file: &str) -> Option<ResultStatus> {
        self.results.get(file).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line() {
        let e = parse_line("[#a1b2c3 1.0MiB/10MiB(10%) CN:8 DL:1.5MiB ETA:1m9s]").unwrap();
        let p = Prog {
            gid: "a1b2c3".into(),
            done: 1 << 20,
            total: 10 << 20,
            conns: Some(8),
            speed: Some(1_572_864),
            eta: Some(69),
        };
        assert_eq!(
            e,
            Event::Progress {
                items: vec![p],
                speed: None
            }
        );
    }

    #[test]
    fn progress_without_eta_and_compact_readout() {
        match parse_line("[#a 0B/0B(0%) CN:1 DL:0B]").unwrap() {
            Event::Progress { items, .. } => {
                assert_eq!((items[0].eta, items[0].speed), (None, Some(0)))
            }
            e => panic!("{e:?}"),
        }
        match parse_line("[DL:2.0MiB][#a 1.0MiB/2.0MiB(50%)][#b 0B/1.0MiB(0%)](+3)").unwrap() {
            Event::Progress { items, speed } => {
                assert_eq!(speed, Some(2 << 20));
                assert_eq!(
                    items.iter().map(|p| p.gid.as_str()).collect::<Vec<_>>(),
                    ["a", "b"]
                );
                assert_eq!(items[0].done, 1 << 20);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn notice_header_file_results_errors_and_noise() {
        assert_eq!(
            parse_line(
                "11/25 00:00:01 [NOTICE] Download complete: /var/cache/pacman/pkg/a.pkg.tar.zst"
            ),
            Some(Event::Complete(
                "/var/cache/pacman/pkg/a.pkg.tar.zst".into()
            ))
        );
        assert_eq!(
            parse_line("*** Download Progress Summary as of Wed Nov 25 ***"),
            Some(Event::BlockStart)
        );
        assert_eq!(
            parse_line("FILE: /x/a.pkg"),
            Some(Event::File("/x/a.pkg".into()))
        );
        assert_eq!(
            parse_line("2089b0|OK  |   1.2MiB/s|/c/a.pkg"),
            Some(Event::Result {
                status: ResultStatus::Ok,
                path: "/c/a.pkg".into()
            })
        );
        assert_eq!(
            parse_line("2089b0|ERR |       0B/s|https://m/b.pkg"),
            Some(Event::Result {
                status: ResultStatus::Error,
                path: "https://m/b.pkg".into()
            })
        );
        assert_eq!(parse_line("gid   |stat|avg speed  |path/URI"), None);
        assert_eq!(
            parse_line("07/22 12:00:00 [ERROR] CUID#7 - Download aborted. URI=https://m/a"),
            Some(Event::Error(
                "CUID#7 - Download aborted. URI=https://m/a".into()
            ))
        );
        assert_eq!(
            parse_line("  -> [HttpSkipResponseCommand.cc:218] errorCode=3 Resource not found"),
            Some(Event::Error("errorCode=3 Resource not found".into()))
        );
        assert_eq!(
            parse_line("\x1b[1;32m*** Download Progress Summary as of x ***\x1b[0m"),
            Some(Event::BlockStart)
        );
        assert_eq!(parse_line("======+====+===========+===="), None);
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn parses_arch_aria2c_single_and_batch_fixtures() {
        let single_file = "go-2:1.27.4-1-x86_64.pkg.tar.zst";
        let mut single = BatchProgress::new([(single_file.to_string(), 44_400_479)]);
        for line in include_str!("../tests/fixtures/aria2-single.txt").lines() {
            single.feed(line);
        }
        assert_eq!(single.result(single_file), Some(true));
        assert!(single.is_completed(single_file));
        assert_eq!(single.done(), 44_400_479);

        let files = [
            "which-2.25-1-x86_64.pkg.tar.zst",
            "gzip-1.15-1-x86_64.pkg.tar.zst",
            "grep-3.12-2-x86_64.pkg.tar.zst",
            "less-1:710-1-x86_64.pkg.tar.zst",
            "file-5.48-1-x86_64.pkg.tar.zst",
            "sed-4.10-2-x86_64.pkg.tar.zst",
            "nano-9.3-1-x86_64.pkg.tar.zst",
        ];
        let mut batch = BatchProgress::new(files.iter().map(|f| ((*f).to_string(), 1)));
        for line in include_str!("../tests/fixtures/aria2-batch.txt").lines() {
            batch.feed(line);
        }
        assert_eq!(batch.completed_count(), files.len());
        assert!(files.iter().all(|f| batch.result(f) == Some(true)));
    }

    #[test]
    fn parses_aria2_next_notice_case_and_result_fixture() {
        let file = "sample.pkg.tar.zst";
        let mut p = BatchProgress::new([(file.to_string(), 245_760)]);
        for line in include_str!("../tests/fixtures/aria2-next-single.txt").lines() {
            p.feed(line);
        }
        assert_eq!(p.result(file), Some(true));
        assert!(p.is_completed(file));
    }

    #[test]
    fn parses_aria2_next_interrupted_result_status() {
        let file = "sample.pkg.tar.zst";
        let mut p = BatchProgress::new([(file.to_string(), 134_217_728)]);
        for line in include_str!("../tests/fixtures/aria2-next-interrupt.txt").lines() {
            p.feed(line);
        }
        assert_eq!(p.result_status(file), Some(ResultStatus::InProgress));
        assert_eq!(p.result(file), Some(false));
    }

    #[test]
    fn parses_arch_aria2c_checksum_and_interrupted_result_fixtures() {
        let file = "sed-4.10-2-x86_64.pkg.tar.zst";
        for fixture in [
            include_str!("../tests/fixtures/aria2-badsha-check.txt"),
            include_str!("../tests/fixtures/aria2-badsha-nocheck.txt"),
        ] {
            let mut p = BatchProgress::new([(file.to_string(), 228_000)]);
            for line in fixture.lines() {
                p.feed(line);
            }
            assert_eq!(p.result(file), Some(false));
            assert!(p
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("Checksum error")));
        }

        let go = "go-2:1.27.4-1-x86_64.pkg.tar.zst";
        let mut interrupted = BatchProgress::new([(go.to_string(), 44_400_479)]);
        for line in include_str!("../tests/fixtures/aria2-interrupt.txt").lines() {
            interrupted.feed(line);
        }
        assert_eq!(interrupted.result(go), Some(false));
        assert_eq!(
            interrupted.result_status(go),
            Some(ResultStatus::InProgress)
        );
        assert!(!interrupted.is_completed(go));
    }

    #[test]
    fn batch_aggregation_replaces_instead_of_adding() {
        let mut b = BatchProgress::new([("a.pkg".to_string(), 100), ("b.pkg".to_string(), 300)]);
        for l in [
            "*** Download Progress Summary as of x ***",
            "[#1 10B/100B(10%) CN:1 DL:1KiB ETA:1s]",
            "FILE: /c/a.pkg",
            "[#2 30B/300B(10%) CN:2 DL:3KiB ETA:1s]",
            "FILE: /c/b.pkg",
            // The same downloads again in a readout line: replaced, not added.
            "[DL:4KiB][#1 20B/100B(20%)][#2 30B/300B(10%)]",
        ] {
            b.feed(l);
        }
        assert_eq!(
            (b.speed(), b.active(), b.conns(), b.done()),
            (4096, 2, 3, 50)
        );
        b.feed("x [NOTICE] Download complete: /c/a.pkg");
        b.feed("x [NOTICE] Download complete: /c/a.pkg");
        b.feed("x [NOTICE] Download complete: /c/unknown.pkg");
        b.feed("[DL:3KiB][#1 100B/100B(100%)][#2 40B/300B(13%)]");
        assert_eq!((b.completed_count(), b.done(), b.active()), (1, 140, 1));
        assert_eq!(b.percent(), 35);
        assert!(b.is_completed("a.pkg") && !b.is_completed("b.pkg"));
        b.feed("abc123|ERR |   0B/s|/c/b.pkg");
        assert_eq!((b.result("b.pkg"), b.result("a.pkg")), (Some(false), None));
    }

    #[test]
    fn single_download_readouts_do_not_double_count() {
        let mut b = BatchProgress::new([("a.pkg".to_string(), 100)]);
        for l in [
            "*** Download Progress Summary as of x ***",
            "[#1 10B/100B(10%) CN:8 DL:1KiB ETA:9s]",
            "FILE: /c/a.pkg",
            "[#1 20B/100B(20%) CN:8 DL:1KiB ETA:8s]",
            "[#1 30B/100B(30%) CN:8 DL:1KiB ETA:7s]",
        ] {
            b.feed(l);
        }
        assert_eq!(
            (b.done(), b.percent(), b.conns(), b.eta()),
            (30, 30, 8, Some(1))
        );
        b.feed("[ERROR] CUID#7 - boom");
        assert_eq!(b.last_error.as_deref(), Some("CUID#7 - boom"));
    }
}
