//! Parsing of `pacman -S[u] --print --print-format "%n %v %s %f %r %l %h"` output.

/// Query format; use this one constant everywhere.
/// name, version, download size, file name, repo, URL (`file://` when cached), sha256.
pub const PRINT_FORMAT: &str = "%n %v %s %f %r %l %h";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    /// Download size as reported by pacman (0 when already cached).
    pub size: u64,
    pub file: String,
    pub repo: String,
    pub url: String,
    /// `None` when pacman gave no (valid) sha256: pacz must not download such a package.
    pub sha256: Option<String>,
}

impl Pkg {
    pub fn is_cached(&self) -> bool {
        self.url.starts_with("file://")
    }
}

fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Tolerant: lines that are not package rows (warnings, blank lines, "there is nothing
/// to do") are skipped instead of aborting the whole run.
pub fn parse_rows(output: &str) -> Vec<Pkg> {
    output.lines().filter_map(parse_row).collect()
}

/// Query parser that fails if a package-shaped line was emitted in an unexpected
/// format. This keeps a pacman format change from silently looking like an empty plan.
pub fn parse_rows_checked(output: &str) -> Result<Vec<Pkg>, String> {
    let mut packages = Vec::new();
    for (index, line) in output.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(pkg) = parse_row(line) {
            packages.push(pkg);
            continue;
        }

        let lower = line.to_ascii_lowercase();
        if lower.starts_with("error:") {
            return Err(format!("pacman reported an error on line {}: {line}", index + 1));
        }
        if lower.starts_with("warning:")
            || lower.starts_with("::")
            || lower.contains("there is nothing to do")
            || [
                "resolving dependencies...",
                "looking for conflicting packages...",
                "checking keyring...",
                "checking package integrity...",
                "loading package files...",
                "checking for file conflicts...",
                "checking available disk space...",
            ]
            .iter()
            .any(|known| lower == *known)
        {
            continue;
        }

        let fields: Vec<&str> = line.split_whitespace().collect();
        let looks_like_package_row = fields.len() >= 5
            && (fields.get(2).is_some_and(|size| size.parse::<u64>().is_ok())
                || fields.get(3).is_some_and(|file| file.contains(".pkg")));
        if looks_like_package_row {
            return Err(format!("could not parse pacman package row on line {}", index + 1));
        }
        return Err(format!("unexpected pacman --print output on line {}: {line}", index + 1));
    }
    Ok(packages)
}

fn parse_row(line: &str) -> Option<Pkg> {
    let mut t = line.split_whitespace();
    let name = t.next()?;
    let version = t.next()?;
    let size: u64 = t.next()?.parse().ok()?;
    let file = t.next()?;
    let repo = t.next()?;
    let url = t.next()?;
    let sha = t.next().filter(|s| valid_sha256(s)).map(str::to_owned);
    if t.next().is_some() {
        return None; // more fields than the format has: not one of our rows
    }
    // The file name is joined onto the cache/staging dir later: never allow path tricks.
    if file.contains('/') || file.contains('\0') || file == ".." || file == "." {
        return None;
    }
    Some(Pkg {
        name: name.into(),
        version: version.into(),
        size,
        file: file.into(),
        repo: repo.into(),
        url: url.into(),
        sha256: sha,
    })
}

/// Test helper shared by the other modules' tests.
#[cfg(test)]
pub fn test_pkg(name: &str, size: u64) -> Pkg {
    let file = format!("{name}.pkg.tar.zst");
    Pkg {
        name: name.into(),
        version: "1-1".into(),
        size,
        url: format!("https://m/{file}"),
        file,
        repo: "core".into(),
        sha256: Some("a".repeat(64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn parses_rows_and_skips_noise() {
        let out = format!(
            "warning: foo-1.0-1 is up to date -- reinstalling\n\
             bash 5.2-1 1500000 bash-5.2-1-x86_64.pkg.tar.zst core https://m/core/bash.pkg {SHA}\n\
             \n\
             there is nothing to do\n\
             curl 1:8.0-1 0 curl-1:8.0-1-x86_64.pkg.tar.zst extra file:///var/cache/pacman/pkg/curl.pkg {SHA}\n"
        );
        let p = parse_rows(&out);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].name.as_str(), p[0].version.as_str(), p[0].size), ("bash", "5.2-1", 1_500_000));
        assert!(!p[0].is_cached());
        assert!(p[1].is_cached());
        assert_eq!(p[1].version, "1:8.0-1");
    }

    #[test]
    fn missing_or_bad_sha_is_none_not_a_parse_failure() {
        let p = parse_rows("a 1-1 1 a.pkg core https://m/a.pkg\nb 1-1 1 b.pkg core https://m/b.pkg nothex\n");
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|x| x.sha256.is_none()));
    }

    #[test]
    fn rejects_path_traversal_and_extra_fields() {
        assert!(parse_rows(&format!("x 1-1 1 ../evil core https://m/x {SHA}")).is_empty());
        assert!(parse_rows(&format!("x 1-1 1 .. core https://m/x {SHA}")).is_empty());
        assert!(parse_rows(&format!("x 1-1 1 x.pkg core https://m/x {SHA} extra")).is_empty());
    }

    #[test]
    fn checked_parser_skips_noise_but_rejects_malformed_package_rows() {
        let row = format!("bash 5.2-1 1500000 bash.pkg.tar.zst core https://m/bash.pkg {SHA}");
        let parsed = parse_rows_checked(&format!("warning: old database\nthere is nothing to do\n{row}\n")).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(parse_rows_checked("bash 5.2-1 nope bash.pkg.tar.zst core https://m/bash.pkg").unwrap_err().contains("line 1"));
        assert!(parse_rows_checked(&format!("bash 5.2-1 1500000 ../bash.pkg core https://m/bash.pkg {SHA}")).is_err());
        assert!(parse_rows_checked("unexpected pacman output").is_err());
        assert!(parse_rows_checked("error: package conflict").is_err());
    }
}
