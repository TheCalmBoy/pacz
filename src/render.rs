//! Terminal rendering as pure functions (no I/O), plus a thread-safe dashboard state.
use crate::aria2::BatchProgress;
use crate::driver::ProgressFn;
use crate::fmt::human;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";
pub const CLEAR_BELOW: &str = "\x1b[J";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowState {
    Waiting,
    Active { pct: u8, done: u64, speed: u64, conns: u32, eta: Option<u64> },
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub state: RowState,
    pub total: u64,
}

pub fn eta(s: u64) -> String {
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub fn bar(pct: u8, width: usize) -> String {
    let filled = (pct.min(100) as usize * width + 50) / 100;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}

/// Char-aware (not byte-aware) truncate and pad.
pub fn fit(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    format!("{t:<n$}")
}

/// Cuts a line to `w` visible characters; escape sequences pass through untouched so
/// a trailing color reset is never lost.
pub fn clip(s: &str, w: usize) -> String {
    let (mut out, mut vis, mut esc) = (String::with_capacity(s.len()), 0usize, false);
    for c in s.chars() {
        if esc {
            out.push(c);
            if c != '[' && ('\x40'..='\x7e').contains(&c) {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
            out.push(c);
        } else if vis < w {
            out.push(c);
            vis += 1;
        }
    }
    out
}

fn layout(width: usize) -> (usize, usize) {
    // (name width, bar width)
    if width >= 110 {
        (26, 18)
    } else if width >= 80 {
        (18, 10)
    } else {
        (12, 5)
    }
}

pub fn paint(s: String, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s
    }
}

/// Green `::` heading, as pacman prints them.
pub fn heading(msg: &str, color: bool) -> String {
    format!("{} {}", paint("::".into(), "1;32", color), paint(msg.into(), "1", color))
}

pub fn render_row(row: &Row, width: usize, color: bool) -> String {
    let (nw, bw) = layout(width);
    let name = paint(fit(&row.label, nw), "36", color);
    let tail = match &row.state {
        RowState::Waiting => "waiting".to_string(),
        RowState::Failed => paint("FAILED".into(), "31", color),
        RowState::Done => paint(format!("complete  {}", human(row.total)), "32", color),
        RowState::Active { pct, done, speed, conns, eta: e } => {
            let sizes = if width >= 80 { format!(" {}/{}", human(*done), human(row.total)) } else { String::new() };
            let e = e.map_or("--".to_string(), eta);
            format!("{} {:>3}%{sizes} {}/s {conns}c {e}", bar(*pct, bw), pct, human(*speed))
        }
    };
    format!("  {name} {tail}")
}

pub fn render_total(pct: u8, done: u64, speed: u64, width: usize) -> String {
    let bw = if width >= 110 { 30 } else if width >= 80 { 18 } else { 10 };
    format!("  Total {} {:>3}%  {} downloaded  {}/s", bar(pct, bw), pct, human(done), human(speed))
}

/// Shared by download workers (writers) and the redraw loop (reader). Rows are keyed by
/// task id, so two batches with the same size can never overwrite each other.
#[derive(Default)]
pub struct Dashboard {
    rows: Mutex<BTreeMap<usize, Row>>,
}

impl Dashboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<usize, Row>> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add(&self, id: usize, label: &str, total: u64) {
        self.lock().insert(id, Row { label: label.to_string(), state: RowState::Waiting, total });
    }

    pub fn set(&self, id: usize, state: RowState) {
        if let Some(r) = self.lock().get_mut(&id) {
            r.state = state;
        }
    }

    pub fn finish(&self, id: usize, ok: bool) {
        self.set(id, if ok { RowState::Done } else { RowState::Failed });
    }

    /// Callback to hand to `Aria2Runner::on_progress`.
    pub fn progress_fn(self: &Arc<Self>) -> ProgressFn {
        let me = Arc::clone(self);
        Box::new(move |id: usize, p: &BatchProgress| {
            let state = RowState::Active { pct: p.percent(), done: p.done(), speed: p.speed(), conns: p.conns(), eta: p.eta() };
            me.set(id, state);
        })
    }

    /// Active rows first, then waiting, failed, done; at most `max_rows` rows (plus a
    /// "more" line) and the total line. Every line fits in `width - 1` columns.
    pub fn frame(&self, width: usize, color: bool, max_rows: usize) -> Vec<String> {
        let rows = self.lock();
        let mut list: Vec<&Row> = rows.values().collect();
        list.sort_by_key(|r| match r.state {
            RowState::Active { .. } => 0,
            RowState::Waiting => 1,
            RowState::Failed => 2,
            RowState::Done => 3,
        });
        let (mut total, mut done, mut speed) = (0u64, 0u64, 0u64);
        for r in &list {
            total += r.total;
            match &r.state {
                RowState::Done => done += r.total,
                RowState::Active { done: d, speed: s, .. } => {
                    done += d;
                    speed += s;
                }
                _ => {}
            }
        }
        let w = width.saturating_sub(1).max(20);
        let shown = list.len().min(max_rows.max(1));
        let mut lines: Vec<String> = list.iter().take(shown).map(|r| format!("\x1b[2K{}", clip(&render_row(r, width, color), w))).collect();
        if list.len() > shown {
            lines.push(format!("\x1b[2K  … {} more", list.len() - shown));
        }
        let pct = (done.min(total) * 100).checked_div(total).map_or(100, |v| v as u8);
        lines.push(format!("\x1b[2K{}", clip(&render_total(pct, done, speed, width), w)));
        lines
    }
}

/// Escape sequence moving the cursor to the start of the line `n` lines up
/// (nothing for 0: `ESC[0F` would move up one line).
pub fn rewind(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("\x1b[{n}F")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_and_bar() {
        assert_eq!((eta(42).as_str(), eta(185).as_str(), eta(3720).as_str()), ("42s", "3m05s", "1h02m"));
        assert_eq!(bar(50, 10), "[#####-----]");
        assert_eq!(bar(0, 4), "[----]");
        assert_eq!(bar(250, 4), "[####]"); // clamped
    }

    #[test]
    fn row_states_wide() {
        let r = |state| Row { label: "linux-cachyos-headers".into(), state, total: 10 << 20 };
        assert!(render_row(&r(RowState::Waiting), 120, false).ends_with("waiting"));
        assert!(render_row(&r(RowState::Failed), 120, false).ends_with("FAILED"));
        assert!(render_row(&r(RowState::Done), 120, false).ends_with("complete  10MiB"));
        let a = render_row(&r(RowState::Active { pct: 50, done: 5 << 20, speed: 2 << 20, conns: 8, eta: Some(185) }), 120, false);
        assert!(a.contains("50%") && a.contains("5MiB/10MiB") && a.contains("2MiB/s") && a.ends_with("8c 3m05s"), "{a}");
    }

    #[test]
    fn narrow_terminal_drops_sizes_and_truncates_unicode_names() {
        let row = Row { label: "ünïcödé-päckage-näme-très-long".into(), state: RowState::Active { pct: 10, done: 1, speed: 1, conns: 1, eta: None }, total: 100 };
        let s = render_row(&row, 60, false);
        assert!(!s.contains('/') || s.contains("B/s"));
        assert!(s.starts_with("  ünïcödé-päck "), "{s}"); // 12 chars, not 12 bytes
    }

    #[test]
    fn clip_counts_visible_chars_and_keeps_escapes() {
        assert_eq!(clip("abcdef", 3), "abc");
        assert_eq!(clip("\x1b[31mabcdef\x1b[0m", 2), "\x1b[31mab\x1b[0m");
        assert_eq!(clip("äöü", 5), "äöü");
    }

    #[test]
    fn dashboard_totals_ordering_and_same_label_batches() {
        let d = Dashboard::new();
        d.add(0, "done1", 100);
        d.add(1, "act", 100);
        d.add(2, "wait", 200);
        d.finish(0, true);
        d.set(1, RowState::Active { pct: 50, done: 50, speed: 10, conns: 1, eta: None });
        let f = d.frame(120, false, 10);
        assert_eq!(f.len(), 4);
        assert!(f[0].contains("act") && f[1].contains("wait") && f[2].contains("done1"));
        assert!(f[3].contains("37%") && f[3].contains("150B downloaded"), "{}", f[3]); // 150/400
        assert_eq!(d.frame(120, false, 1).len(), 3, "one row, a 'more' line, the total");
        d.finish(1, false);
        assert!(d.frame(120, false, 10).iter().any(|l| l.contains("FAILED")));
        // Two tasks with the same label stay two rows.
        d.add(3, "same", 1);
        d.add(4, "same", 1);
        assert_eq!(d.frame(120, false, 10).len(), 6);
        assert!(d.frame(30, false, 10).iter().all(|l| l.trim_start_matches("\x1b[2K").chars().count() <= 29));
    }

    #[test]
    fn progress_fn_updates_dashboard() {
        let d = Dashboard::new();
        d.add(7, "batch 1/1 (1 pkgs)", 100);
        let f = d.progress_fn();
        let mut p = BatchProgress::new([("a.pkg".to_string(), 100)]);
        p.feed("x [NOTICE] Download complete: /c/a.pkg");
        f(7, &p);
        assert!(d.frame(120, false, 5)[0].contains("100%"));
    }

    #[test]
    fn rewind_zero_is_empty() {
        assert_eq!(rewind(0), "");
        assert_eq!(rewind(3), "\x1b[3F");
    }
}
