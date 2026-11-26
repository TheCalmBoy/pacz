//! AUR step helpers (pure): parsing `paru -Qua`, building `paru -Sua` arguments, and
//! the environment for running paru as the invoking sudo user.
use crate::fmt::strip_ansi;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AurUpdate {
    pub name: String,
    pub old: String,
    pub new: String,
}

/// `name old -> new` lines; ignored packages (`[ignored]`) and noise are skipped.
pub fn parse_qua(out: &str) -> Vec<AurUpdate> {
    out.lines()
        .filter_map(|l| {
            let l = strip_ansi(l);
            if l.contains("[ignored]") {
                return None;
            }
            let t: Vec<&str> = l.split_whitespace().collect();
            (t.len() >= 4 && t[2] == "->").then(|| AurUpdate { name: t[0].into(), old: t[1].into(), new: t[3].into() })
        })
        .collect()
}

pub fn qua_args() -> Vec<String> {
    vec!["-Qua".into()]
}

pub fn sua_args(noconfirm: bool, ignore: &[String]) -> Vec<String> {
    let mut a = vec!["-Sua".to_string()];
    if noconfirm {
        a.push("--noconfirm".into());
    }
    if !ignore.is_empty() {
        a.push(format!("--ignore={}", ignore.join(",")));
    }
    a
}

/// Validates `SUDO_USER`: AUR packages must never be built as root.
pub fn sudo_user(v: Option<&str>) -> Result<&str, String> {
    match v.map(str::trim) {
        Some(u) if !u.is_empty() && u != "root" => Ok(u),
        _ => Err("the AUR step runs paru as your own user, but SUDO_USER is empty or root; \
                  run pacz with sudo from your user account"
            .into()),
    }
}

/// Environment for paru: (variables to set, variables to remove).
pub fn user_env(name: &str, home: &str, current: impl IntoIterator<Item = String>) -> (Vec<(String, String)>, Vec<String>) {
    let set = vec![
        ("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string()),
        ("HOME".to_string(), home.to_string()),
        ("USER".to_string(), name.to_string()),
        ("LOGNAME".to_string(), name.to_string()),
    ];
    let remove = current.into_iter().filter(|k| k.starts_with("SUDO_")).collect();
    (set, remove)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_updates_skips_ignored_and_noise() {
        let out = "\x1b[1mfoo-git\x1b[0m r10.abc-1 -> r12.def-1\nbar 1.0-1 -> 1.1-1 [ignored]\n:: Looking for devel updates...\nbaz 2-1 -> 3-1\n";
        let u = parse_qua(out);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0], AurUpdate { name: "foo-git".into(), old: "r10.abc-1".into(), new: "r12.def-1".into() });
        assert_eq!(u[1].name, "baz");
    }

    #[test]
    fn sua_arguments() {
        assert_eq!(sua_args(false, &[]), ["-Sua"]);
        assert_eq!(sua_args(true, &["a".into(), "b".into()]), ["-Sua", "--noconfirm", "--ignore=a,b"]);
    }

    #[test]
    fn sudo_user_must_be_a_real_user() {
        assert_eq!(sudo_user(Some("alice")), Ok("alice"));
        assert!(sudo_user(Some("root")).is_err());
        assert!(sudo_user(Some(" ")).is_err());
        assert!(sudo_user(None).is_err());
    }

    #[test]
    fn env_sets_user_vars_and_drops_sudo_ones() {
        let (set, rm) = user_env("alice", "/home/alice", ["SUDO_USER".to_string(), "TERM".to_string(), "SUDO_UID".to_string()]);
        assert!(set.contains(&("HOME".to_string(), "/home/alice".to_string())));
        assert!(set.contains(&("LOGNAME".to_string(), "alice".to_string())));
        assert_eq!(rm, ["SUDO_USER", "SUDO_UID"]);
    }
}
