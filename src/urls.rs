//! Choosing download sources for a package.
use crate::conf::Repo;
use crate::rows::Pkg;

#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// Already in pacman's cache.
    Cached,
    /// Mirrors to give aria2 (all serve the same file).
    Urls(Vec<String>),
    /// No usable source; the package must be reported as failed.
    None,
}

/// Pacman's own chosen URL (`%l`) goes first, then the repo's other mirrors.
/// If the repo is unknown or has no http(s) servers, pacman's URL is used alone.
pub fn resolve(repos: &[Repo], pkg: &Pkg) -> Source {
    if pkg.is_cached() {
        return Source::Cached;
    }
    let is_http = |u: &str| u.starts_with("http://") || u.starts_with("https://");
    let mut urls: Vec<String> = Vec::new();
    if is_http(&pkg.url) {
        urls.push(pkg.url.clone());
    }
    if let Some(r) = repos.iter().find(|r| r.name == pkg.repo) {
        for s in &r.servers {
            let u = format!("{s}/{}", pkg.file);
            if !urls.contains(&u) {
                urls.push(u);
            }
        }
    }
    if urls.is_empty() {
        Source::None
    } else {
        Source::Urls(urls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::test_pkg;

    fn pkg(url: &str, repo: &str) -> Pkg {
        Pkg { url: url.into(), repo: repo.into(), file: "p.pkg".into(), sha256: None, ..test_pkg("p", 1) }
    }
    fn repo() -> Vec<Repo> {
        vec![Repo { name: "core".into(), servers: vec!["https://a".into(), "https://b".into()] }]
    }
    #[test]
    fn cached() {
        assert_eq!(resolve(&repo(), &pkg("file:///x", "core")), Source::Cached);
    }
    #[test]
    fn own_url_first_and_deduped() {
        let s = resolve(&repo(), &pkg("https://b/p.pkg", "core"));
        assert_eq!(s, Source::Urls(vec!["https://b/p.pkg".into(), "https://a/p.pkg".into()]));
    }
    #[test]
    fn unknown_repo_falls_back_to_own_url() {
        assert_eq!(resolve(&repo(), &pkg("https://z/p.pkg", "custom")), Source::Urls(vec!["https://z/p.pkg".into()]));
        assert_eq!(resolve(&repo(), &pkg("ftp://z/p.pkg", "custom")), Source::None);
    }
}
