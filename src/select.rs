//! Update selector, shared by the repository and AUR lists. Pure state machine; the
//! prompt loop lives in `app.rs`.
//!
//! Every entry has a fixed number: its rank by download size (largest = 1), or by name
//! when there are no sizes (AUR). The number never changes when the view is re-sorted
//! or filtered, so "ignore 3" always means the same package.
//!
//! Input: numbers/ranges (`1 3 5-7`, commas ok) toggle ignore, `s`/`n` sort by size/name,
//! `/text` search (`/` alone clears), `c` clears all ignores, Enter continues, `q` quits.
use crate::fmt::human;
use crate::render::{fit, heading, paint};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub detail: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Size,
    Name,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// State changed: show the list again and ask again.
    Again,
    /// Ask again without reprinting the list (the message explains why).
    Msg(String),
    /// Enter: continue with the current selection.
    Done,
    /// Abort the operation.
    Quit,
}

pub struct Selector {
    /// In rank order: rank = index + 1.
    items: Vec<Item>,
    ignored: Vec<bool>,
    pub sort: Sort,
    pub filter: String,
}

impl Selector {
    pub fn new(mut items: Vec<Item>) -> Self {
        items.sort_by(|a, b| {
            b.size
                .unwrap_or(0)
                .cmp(&a.size.unwrap_or(0))
                .then_with(|| a.name.cmp(&b.name))
        });
        let has_size = items.iter().any(|i| i.size.is_some());
        let n = items.len();
        Self {
            items,
            ignored: vec![false; n],
            sort: if has_size { Sort::Size } else { Sort::Name },
            filter: String::new(),
        }
    }

    pub fn has_sizes(&self) -> bool {
        self.items.iter().any(|i| i.size.is_some())
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
    }

    pub fn toggle_rank(&mut self, rank: usize) -> bool {
        if rank == 0 || rank > self.ignored.len() {
            return false;
        }
        self.ignored[rank - 1] = !self.ignored[rank - 1];
        true
    }

    pub fn clear_ignored(&mut self) {
        self.ignored.fill(false);
    }

    /// Names the user chose to ignore.
    pub fn ignored(&self) -> Vec<String> {
        self.items
            .iter()
            .zip(&self.ignored)
            .filter(|&(_, &ign)| ign)
            .map(|(it, _)| it.name.clone())
            .collect()
    }

    pub fn download_total(&self) -> u64 {
        self.items
            .iter()
            .zip(&self.ignored)
            .filter(|&(_, ignored)| !*ignored)
            .filter_map(|(item, _)| item.size)
            .sum()
    }

    /// Visible rows: (rank, item, ignored), in the current sort, filtered.
    pub fn view(&self) -> Vec<(usize, &Item, bool)> {
        let f = self.filter.to_lowercase();
        let mut v: Vec<(usize, &Item, bool)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| f.is_empty() || it.name.to_lowercase().contains(&f))
            .map(|(i, it)| (i + 1, it, self.ignored[i]))
            .collect();
        if self.sort == Sort::Name {
            v.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        }
        v
    }

    pub fn apply(&mut self, input: &str) -> Step {
        let s = input.trim();
        match s {
            "" => return Step::Done,
            "q" | "Q" => return Step::Quit,
            "n" | "N" => {
                self.sort = Sort::Name;
                return Step::Again;
            }
            "s" | "S" => {
                if !self.has_sizes() {
                    return Step::Msg(
                        "these entries have no sizes; they are listed by name".into(),
                    );
                }
                self.sort = Sort::Size;
                return Step::Again;
            }
            "c" | "C" => {
                self.clear_ignored();
                return Step::Again;
            }
            _ => {}
        }
        if let Some(q) = s.strip_prefix('/') {
            self.filter = q.trim().to_string();
            return Step::Again;
        }
        let mut picks = BTreeSet::new();
        for tok in s
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
        {
            let (a, b) = tok.split_once('-').unwrap_or((tok, tok));
            let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) else {
                return Step::Msg(format!(
                    "not understood: {tok:?} (numbers, s, n, /text, c, Enter or q)"
                ));
            };
            let (lo, hi) = (a.min(b), a.max(b));
            if lo == 0 || hi > self.items.len() {
                return Step::Msg(format!("no entry {tok} (valid: 1-{})", self.items.len()));
            }
            picks.extend(lo..=hi);
        }
        for r in picks {
            self.toggle_rank(r);
        }
        Step::Again
    }

    pub fn render(&self, title: &str, color: bool) -> Vec<String> {
        let view = self.view();
        let nw = self
            .items
            .iter()
            .map(|i| i.name.chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 40);
        let dw = self
            .items
            .iter()
            .map(|i| i.detail.chars().count())
            .max()
            .unwrap_or(0)
            .min(48);
        let rw = self.items.len().to_string().len();
        let sort = match self.sort {
            Sort::Size => "size",
            Sort::Name => "name",
        };
        let filter = if self.filter.is_empty() {
            String::new()
        } else {
            format!(", matching \"{}\"", self.filter)
        };
        let mut out = vec![heading(
            &format!("{title}: {} (sorted by {sort}{filter})", self.items.len()),
            color,
        )];
        for (rank, it, ign) in &view {
            let size = it.size.map_or(String::new(), human);
            let mark = if *ign {
                paint("ignore".into(), "31", color)
            } else {
                "      ".to_string()
            };
            out.push(format!(
                "  {rank:>rw$}  {mark}  {}  {}  {size:>9}",
                fit(&it.name, nw),
                fit(&it.detail, dw)
            ));
        }
        if view.is_empty() {
            out.push("  (no matches)".into());
        }
        let ign = self.ignored.iter().filter(|&&x| x).count();
        let mut summary = format!("{} selected, {ign} ignored", self.items.len() - ign);
        if self.has_sizes() {
            let total: u64 = self
                .items
                .iter()
                .zip(&self.ignored)
                .filter(|&(_, &i)| !i)
                .filter_map(|(it, _)| it.size)
                .sum();
            summary.push_str(&format!(", download {}", human(total)));
        }
        out.push(heading(&summary, color));
        out.push("   numbers/ranges toggle ignore (1 3 5-7) · s/n sort by size/name · /text search · c clear · Enter continue · q quit".into());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Item> {
        [("small", 10), ("huge", 1000), ("mid", 100), ("alpha", 100)]
            .into_iter()
            .map(|(n, s)| Item {
                name: n.into(),
                detail: "1 -> 2".into(),
                size: Some(s),
            })
            .collect()
    }

    #[test]
    fn ranks_are_by_size_and_stable_across_sorting_and_search() {
        let mut s = Selector::new(items());
        let ranks = |s: &Selector| {
            s.view()
                .iter()
                .map(|(r, it, _)| (*r, it.name.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ranks(&s),
            [
                (1, "huge".into()),
                (2, "alpha".into()),
                (3, "mid".into()),
                (4, "small".into())
            ]
        );
        assert_eq!(s.apply("n"), Step::Again);
        assert_eq!(ranks(&s)[0], (2, "alpha".to_string()));
        assert_eq!(s.apply("/MI"), Step::Again);
        assert_eq!(ranks(&s), [(3, "mid".to_string())]);
        assert_eq!(s.apply("/"), Step::Again);
        assert_eq!(s.view().len(), 4);
    }

    #[test]
    fn toggles_ranges_and_errors() {
        let mut s = Selector::new(items());
        assert_eq!(s.apply("1 3-4"), Step::Again);
        assert_eq!(s.ignored(), ["huge", "mid", "small"]);
        assert_eq!(s.apply("3,1"), Step::Again);
        assert_eq!(s.ignored(), ["small"]);
        assert!(matches!(s.apply("9"), Step::Msg(_)));
        assert!(matches!(s.apply("0"), Step::Msg(_)));
        assert!(matches!(s.apply("x"), Step::Msg(_)));
        assert_eq!(s.ignored(), ["small"], "errors change nothing");
        assert_eq!(s.apply("c"), Step::Again);
        assert!(s.ignored().is_empty());
        assert_eq!(s.apply(""), Step::Done);
        assert_eq!(s.apply("q"), Step::Quit);
    }

    #[test]
    fn aur_lists_without_sizes_rank_by_name() {
        let it = |n: &str| Item {
            name: n.into(),
            detail: "1 -> 2".into(),
            size: None,
        };
        let mut s = Selector::new(vec![it("zeta"), it("beta")]);
        assert_eq!(s.sort, Sort::Name);
        assert_eq!(s.view()[0].1.name, "beta");
        assert!(matches!(s.apply("s"), Step::Msg(_)));
        assert!(!s
            .render("AUR updates", false)
            .iter()
            .any(|l| l.contains("download")));
    }

    #[test]
    fn render_shows_marks_and_totals() {
        let mut s = Selector::new(items());
        s.apply("1");
        let lines = s.render("Repository updates", false);
        assert!(lines[0].starts_with(":: Repository updates: 4 (sorted by size)"));
        assert!(lines[1].contains("ignore") && lines[1].contains("huge"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("3 selected, 1 ignored, download 210B")),
            "{lines:?}"
        );
    }
}
