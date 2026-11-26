//! `/etc/pacz.conf`: `key = value` lines, `#` comments. Unknown keys are errors, so a
//! typo never silently falls back to a default.
use crate::fmt::parse_size;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub pacman_bin: String,
    pub aria2_bin: String,
    pub paru_bin: String,
    /// Default: `--cachedir`, else pacman.conf's first `CacheDir`, else /var/cache/pacman/pkg.
    pub cache_dir: Option<PathBuf>,
    /// Default: `pacz-staging` next to the cache dir (same filesystem, so moves are renames).
    pub staging_dir: Option<PathBuf>,
    /// Passed as `--state-dir` to aria2-next; `none` disables it (for stock aria2c).
    pub state_dir: Option<PathBuf>,
    pub tmp_dir: PathBuf,
    pub workers: usize,
    pub mirrors: usize,
    pub small_bytes: u64,
    pub batch_max: usize,
    pub batch_min: usize,
    pub check_integrity: bool,
    pub aria2_args: Vec<String>,
    /// Packages without a sha256 in the sync DB: let pacman download them (it verifies
    /// signatures) instead of failing.
    pub fallback_to_pacman: bool,
    pub aur: bool,
    /// Forward `--noconfirm` to paru (skips PKGBUILD review).
    pub aur_noconfirm: bool,
    pub log_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pacman_bin: "pacman".into(),
            aria2_bin: "aria2-next".into(),
            paru_bin: "paru".into(),
            cache_dir: None,
            staging_dir: None,
            state_dir: Some(PathBuf::from("/var/lib/aria2-next-pacman")),
            tmp_dir: PathBuf::from("/tmp"),
            workers: 12,
            mirrors: 5,
            small_bytes: 8 << 20,
            batch_max: 40,
            batch_min: 3,
            check_integrity: true,
            aria2_args: vec![],
            fallback_to_pacman: true,
            aur: true,
            aur_noconfirm: true,
            log_file: None,
        }
    }
}

fn boolean(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn path_or_none(v: &str) -> Option<PathBuf> {
    (!v.is_empty() && v != "none").then(|| PathBuf::from(v))
}

pub fn parse_config(text: &str) -> Result<Config, String> {
    let mut c = Config::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let err = |m: &str| format!("line {}: {m}", n + 1);
        let (k, v) = line.split_once('=').ok_or_else(|| err("expected key = value"))?;
        let (k, v) = (k.trim(), v.trim());
        let num = |v: &str| v.parse::<usize>().ok().filter(|&x| x > 0).ok_or_else(|| err("expected a positive number"));
        let flag = |v: &str| boolean(v).ok_or_else(|| err("expected true or false"));
        match k {
            "pacman" => c.pacman_bin = v.into(),
            "aria2" => c.aria2_bin = v.into(),
            "paru" => c.paru_bin = v.into(),
            "cache_dir" => c.cache_dir = path_or_none(v),
            "staging_dir" => c.staging_dir = path_or_none(v),
            "state_dir" => c.state_dir = path_or_none(v),
            "tmp_dir" => c.tmp_dir = PathBuf::from(v),
            "workers" => c.workers = num(v)?,
            "mirrors" => c.mirrors = num(v)?,
            "batch_max" => c.batch_max = num(v)?,
            "batch_min" => c.batch_min = num(v)?,
            "small_size" => c.small_bytes = parse_size(v).ok_or_else(|| err("expected a size like 8MiB"))?,
            "check_integrity" => c.check_integrity = flag(v)?,
            "aria2_args" => c.aria2_args = v.split_whitespace().map(String::from).collect(),
            "fallback_to_pacman" => c.fallback_to_pacman = flag(v)?,
            "aur" => c.aur = flag(v)?,
            "aur_noconfirm" => c.aur_noconfirm = flag(v)?,
            "log_file" => c.log_file = path_or_none(v),
            _ => return Err(err(&format!("unknown key {k:?}"))),
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(parse_config("# nothing\n\n").unwrap(), Config::default());
        let c = parse_config("workers = 4\nsmall_size = 2MiB\nstate_dir = none\naur = no\naria2 = aria2c\naria2_args = --async-dns=false -x2\n").unwrap();
        assert_eq!((c.workers, c.small_bytes, c.state_dir.clone(), c.aur), (4, 2 << 20, None, false));
        assert_eq!((c.aria2_bin.as_str(), c.aria2_args.len()), ("aria2c", 2));
    }

    #[test]
    fn errors_name_the_line() {
        assert!(parse_config("workers = 0").unwrap_err().contains("line 1"));
        assert!(parse_config("\nworkrs = 3").unwrap_err().contains("line 2"));
        assert!(parse_config("aur = maybe").is_err());
        assert!(parse_config("just text").is_err());
    }
}
