//! pacman.conf parsing: per-repo mirror lists from `Server` and `Include` lines,
//! `[options]` values pacz needs, and the XferCommand-free copy for the DB refresh.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub name: String,
    pub servers: Vec<String>,
}

/// The `[options]` values pacz cares about.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub cache_dirs: Vec<String>,
    pub arch: Vec<String>,
}

fn key_value(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    Some((k.trim(), v.trim()))
}

fn section(line: &str) -> Option<&str> {
    line.trim().strip_prefix('[').and_then(|s| s.strip_suffix(']')).map(str::trim)
}

fn expand(url: &str, repo: &str, arch: &str) -> Option<String> {
    // `$arch` also covers CachyOS's `$arch_v3`/`$arch_v4` ("x86_64" + "_v3").
    let url = url.replace("$repo", repo).replace("$arch", arch);
    let url = url.trim_end_matches('/').to_owned();
    // aria2 can fetch http(s) mirrors; file:// repos are handled as "cached".
    (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
}

/// `read_include` maps an Include path (possibly a glob) to its contents (injected for tests).
/// At most `max` distinct servers are kept per repo, in file order.
pub fn parse_conf(text: &str, arch: &str, read_include: &dyn Fn(&str) -> Option<String>, max: usize) -> Vec<Repo> {
    let mut repos: Vec<Repo> = Vec::new();
    let mut in_repo = false;
    let add = |repos: &mut Vec<Repo>, raw: &str| {
        if let Some(r) = repos.last_mut() {
            if r.servers.len() < max {
                if let Some(u) = expand(raw, &r.name, arch) {
                    if !r.servers.contains(&u) {
                        r.servers.push(u);
                    }
                }
            }
        }
    };
    for line in text.lines() {
        if let Some(name) = section(line) {
            in_repo = name != "options";
            if in_repo {
                repos.push(Repo { name: name.into(), servers: vec![] });
            }
            continue;
        }
        if !in_repo {
            continue;
        }
        match key_value(line) {
            Some(("Server", v)) => add(&mut repos, v),
            Some(("Include", path)) => {
                if let Some(body) = read_include(path) {
                    for l in body.lines() {
                        if let Some(("Server", v)) = key_value(l) {
                            add(&mut repos, v);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    repos
}

pub fn parse_options(text: &str) -> Options {
    let mut o = Options::default();
    let mut in_opts = false;
    for line in text.lines() {
        if let Some(name) = section(line) {
            in_opts = name == "options";
            continue;
        }
        if !in_opts {
            continue;
        }
        match key_value(line) {
            Some(("CacheDir", v)) => o.cache_dirs.extend(v.split_whitespace().map(String::from)),
            Some(("Architecture", v)) => o.arch.extend(v.split_whitespace().map(String::from)),
            _ => {}
        }
    }
    o
}

/// Architecture used for `$arch`: the first configured one, or `native` for `auto`/none.
pub fn expansion_arch(o: &Options, native: &str) -> String {
    o.arch.iter().find(|a| a.as_str() != "auto").cloned().unwrap_or_else(|| native.to_string())
}

/// pacman.conf with every `XferCommand` line commented out, so the database refresh
/// uses pacman's built-in downloader (its progress UI and signature handling).
pub fn strip_xfer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if matches!(key_value(line), Some(("XferCommand", _))) {
            out.push_str("# (XferCommand disabled by pacz for the database refresh)\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// `*` and `?` wildcards, as pacman allows in `Include` file names.
pub fn wildcard_match(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inc(p: &str) -> Option<String> {
        (p == "/etc/pacman.d/mirrorlist").then(|| {
            "# comment\nServer = https://a.example/$repo/os/$arch/\nServer=https://b.example/$repo/os/$arch\n\
             Server = ftp://skipped.example/x\n"
                .to_string()
        })
    }

    #[test]
    fn include_any_spacing_and_expansion() {
        let conf = "[options]\nSigLevel = Required\n[core]\nInclude=/etc/pacman.d/mirrorlist\n[extra]\n  Include   =   /etc/pacman.d/mirrorlist  \n";
        let r = parse_conf(conf, "x86_64", &inc, 5);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].servers, ["https://a.example/core/os/x86_64", "https://b.example/core/os/x86_64"]);
        assert_eq!(r[1].name, "extra");
    }

    #[test]
    fn direct_server_lines_and_cap() {
        let conf = "[custom]\nServer = https://1/$repo\nServer = https://2/$repo\nServer = https://3/$repo\n";
        let r = parse_conf(conf, "x86_64", &|_| None, 2);
        assert_eq!(r[0].servers, ["https://1/custom", "https://2/custom"]);
    }

    #[test]
    fn missing_include_or_empty_repo_does_not_panic() {
        let r = parse_conf("[core]\nInclude = /nope\n[empty]\n", "x86_64", &|_| None, 5);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.servers.is_empty()));
    }

    #[test]
    fn cachyos_arch_v3_expands() {
        let r = parse_conf("[cachyos-v3]\nServer = https://m/repo/$arch_v3/$repo\n", "x86_64", &|_| None, 5);
        assert_eq!(r[0].servers, ["https://m/repo/x86_64_v3/cachyos-v3"]);
    }

    #[test]
    fn options_cache_dir_and_arch() {
        let o = parse_options("[options]\nCacheDir = /a/ /b/\nCacheDir=/c\nArchitecture = x86_64 x86_64_v3\n[core]\nCacheDir = /ignored\n");
        assert_eq!(o.cache_dirs, ["/a/", "/b/", "/c"]);
        assert_eq!(expansion_arch(&o, "aarch64"), "x86_64");
        assert_eq!(expansion_arch(&parse_options("[options]\nArchitecture = auto\n"), "aarch64"), "aarch64");
    }

    #[test]
    fn xfer_command_is_disabled_and_rest_kept() {
        let s = strip_xfer("[options]\nXferCommand = /usr/bin/curl -o %o %u\n#XferCommand = x\nHoldPkg = pacman\n");
        assert!(!s.lines().any(|l| l.starts_with("XferCommand")));
        assert!(s.contains("#XferCommand = x") && s.contains("HoldPkg = pacman"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.conf", "a.conf"));
        assert!(wildcard_match("mirror?ist", "mirrorlist"));
        assert!(wildcard_match("*", ""));
        assert!(!wildcard_match("*.conf", "a.conf.bak"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
    }
}
