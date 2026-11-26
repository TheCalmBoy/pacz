//! Command-line parsing and pacman argument building. No shell is ever involved:
//! every argument is passed to `Command` as its own argv entry, and targets always
//! follow a `--` so a target can never be read as an option.
use crate::rows::PRINT_FORMAT;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `-S`, `-Sy`, `-Su`, `-Syu`, `-Sw`, ... (also as `-S -y -u` or long options).
    Sync { refresh: u8, upgrade: bool, download_only: bool },
    /// `-R...`: needs root, handed to pacman unchanged.
    Remove,
    /// Anything else (`-Q`, `-Ss`, `-Si`, `-Sc`, `-U`, `-F`, `-S --help`, ...):
    /// handed to pacman unchanged.
    Pacman,
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub op: Op,
    pub targets: Vec<String>,
    pub ignore: Vec<String>,
    pub noconfirm: bool,
    /// pacz-only: show the plan; refresh, download and install nothing.
    pub dry_run: bool,
    /// Every other option, forwarded to the refresh, the query and the install.
    pub passthrough: Vec<String>,
    /// The arguments as given (minus pacz-only ones), for `Op::Remove` / `Op::Pacman`.
    pub raw: Vec<String>,
}

/// Long options that take a separate value argument.
pub const VALUE_FLAGS: &[&str] = &[
    "--overwrite",
    "--ignoregroup",
    "--config",
    "--root",
    "--dbpath",
    "--cachedir",
    "--sysroot",
    "--assume-installed",
    "--color",
    "--gpgdir",
    "--logfile",
    "--hookdir",
    "--arch",
    "--print-format",
];
/// Short options that take a value: `-b` (`--dbpath`), `-r` (`--root`).
const SHORT_VALUE: &[char] = &['b', 'r'];
/// Sync letters that make `-S` something pacman should do itself (search, info, ...).
const SYNC_PASS: &[char] = &['s', 'i', 'l', 'g', 'c', 'p'];
const SYNC_PASS_LONG: &[&str] = &["--search", "--info", "--list", "--groups", "--clean", "--print", "--print-format"];

fn long_op(a: &str) -> Option<char> {
    Some(match a {
        "--sync" => 'S',
        "--remove" => 'R',
        "--query" => 'Q',
        "--upgrade" => 'U',
        "--files" => 'F',
        "--database" => 'D',
        "--deptest" => 'T',
        "--version" => 'V',
        _ => return None,
    })
}

fn set_op(slot: &mut Option<char>, c: char) -> Result<(), String> {
    match *slot {
        Some(o) if o != c => Err("only one operation may be used at a time".into()),
        _ => {
            *slot = Some(c);
            Ok(())
        }
    }
}

fn split_list(v: &str) -> impl Iterator<Item = String> + '_ {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

pub fn parse_cli<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
    let args: Vec<String> = args.into_iter().collect();
    if args.is_empty() {
        return Err("no operation given (try pacz --help)".into());
    }
    let mut cli = Cli { op: Op::Help, targets: vec![], ignore: vec![], noconfirm: false, dry_run: false, passthrough: vec![], raw: vec![] };
    let (mut op, mut help) = (None::<char>, false);
    let (mut refresh, mut upgrade, mut download_only, mut pass) = (0u8, false, false, false);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        i += 1;
        if a == "--" {
            cli.raw.push(a);
            cli.raw.extend(args[i..].iter().cloned());
            cli.targets.extend(args[i..].iter().cloned());
            break;
        }
        if a == "--dry-run" {
            cli.dry_run = true;
            continue;
        }
        cli.raw.push(a.clone());
        if let Some(c) = long_op(&a) {
            set_op(&mut op, c)?;
        } else if a == "--help" {
            help = true;
        } else if a == "--refresh" {
            refresh = refresh.saturating_add(1);
        } else if a == "--sysupgrade" {
            upgrade = true;
        } else if a == "--downloadonly" {
            download_only = true;
        } else if a == "--noconfirm" {
            cli.noconfirm = true;
        } else if a == "--ignore" {
            let v = args.get(i).ok_or("--ignore needs a value")?.clone();
            i += 1;
            cli.raw.push(v.clone());
            cli.ignore.extend(split_list(&v));
        } else if let Some(v) = a.strip_prefix("--ignore=") {
            cli.ignore.extend(split_list(v));
        } else if a.starts_with("--") {
            let name = a.split('=').next().unwrap_or(&a);
            if SYNC_PASS_LONG.contains(&name) {
                pass = true;
            }
            if VALUE_FLAGS.contains(&a.as_str()) {
                let v = args.get(i).ok_or_else(|| format!("{a} needs a value"))?.clone();
                i += 1;
                cli.raw.push(v.clone());
                cli.passthrough.extend([a, v]);
            } else {
                cli.passthrough.push(a);
            }
        } else if a.len() > 1 && a.starts_with('-') {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut keep = String::new();
            let mut j = 0;
            while j < chars.len() {
                let c = chars[j];
                j += 1;
                match c {
                    'S' | 'R' | 'Q' | 'U' | 'F' | 'D' | 'T' | 'V' => set_op(&mut op, c)?,
                    'h' => help = true,
                    'y' => refresh = refresh.saturating_add(1),
                    'u' => upgrade = true,
                    'w' => download_only = true,
                    c if SHORT_VALUE.contains(&c) => {
                        let rest: String = chars[j..].iter().collect();
                        j = chars.len();
                        let v = if rest.is_empty() {
                            let v = args.get(i).ok_or_else(|| format!("-{c} needs a value"))?.clone();
                            i += 1;
                            cli.raw.push(v.clone());
                            v
                        } else {
                            rest
                        };
                        cli.passthrough.extend([format!("-{c}"), v]);
                    }
                    c => {
                        if SYNC_PASS.contains(&c) {
                            pass = true;
                        }
                        keep.push(c);
                    }
                }
            }
            if !keep.is_empty() {
                cli.passthrough.push(format!("-{keep}"));
            }
        } else {
            cli.targets.push(a);
        }
    }
    cli.op = match op {
        None if help => Op::Help,
        None => return Err("no operation given (try pacz --help)".into()),
        Some('V') => Op::Version,
        Some(_) if help => Op::Pacman,
        Some('S') if pass => Op::Pacman,
        Some('S') => Op::Sync { refresh, upgrade, download_only },
        Some('R') => Op::Remove,
        Some(_) => Op::Pacman,
    };
    Ok(cli)
}

/// Value of the last `--name VALUE` or `--name=VALUE` in `args`.
pub fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let mut found = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            found = it.next().map(String::as_str);
        } else if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            found = Some(v);
        }
    }
    found
}

/// `args` without the given value options (both `--x V` and `--x=V` forms).
pub fn without_flags(args: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if names.contains(&a.as_str()) {
            it.next();
        } else if !names.iter().any(|n| a.strip_prefix(*n).is_some_and(|r| r.starts_with('='))) {
            out.push(a.clone());
        }
    }
    out
}

/// Only the given value options (with their values).
pub fn only_flags(args: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if names.contains(&a.as_str()) {
            out.push(a.clone());
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        } else if names.iter().any(|n| a.strip_prefix(*n).is_some_and(|r| r.starts_with('='))) {
            out.push(a.clone());
        }
    }
    out
}

fn build(cli: &Cli, passthrough: Vec<String>, extra: &[&str]) -> Result<Vec<String>, String> {
    let Op::Sync { upgrade, .. } = cli.op else { return Err("not a sync operation".into()) };
    let mut a = vec![if upgrade { "-Su" } else { "-S" }.to_string()];
    if !cli.ignore.is_empty() {
        a.push(format!("--ignore={}", cli.ignore.join(",")));
    }
    a.extend(passthrough);
    a.extend(extra.iter().map(|s| s.to_string()));
    if !cli.targets.is_empty() {
        a.push("--".into());
        a.extend(cli.targets.iter().cloned());
    }
    Ok(a)
}

/// `pacman` argv for the size/URL query (no refresh: that is a separate step).
pub fn query_args(cli: &Cli) -> Result<Vec<String>, String> {
    build(cli, without_flags(&cli.passthrough, &["--color"]), &["--color", "never", "--print", "--print-format", PRINT_FORMAT])
}

/// `pacman` argv for the real install, run after all downloads succeeded. Color is left
/// to pacman (`auto` by default): its output goes straight to the user's terminal.
pub fn install_args(cli: &Cli) -> Result<Vec<String>, String> {
    let extra: &[&str] = if cli.noconfirm { &["--noconfirm"] } else { &[] };
    build(cli, cli.passthrough.clone(), extra)
}

/// pacman argv for completing a download-only request when pacz cannot verify a
/// package from the sync database itself.
pub fn download_only_args(cli: &Cli) -> Result<Vec<String>, String> {
    let mut a = install_args(cli)?;
    let at = a.iter().position(|arg| arg == "--").unwrap_or(a.len());
    a.insert(at, "--downloadonly".into());
    Ok(a)
}

/// `pacman -Sy[y]` argv using the XferCommand-free copy of pacman.conf at `conf`.
pub fn refresh_args(cli: &Cli, conf: &str) -> Result<Vec<String>, String> {
    let Op::Sync { refresh, .. } = cli.op else { return Err("not a sync operation".into()) };
    let mut a = vec![format!("-S{}", "y".repeat(refresh.clamp(1, 2) as usize))];
    a.extend(without_flags(&cli.passthrough, &["--config"]));
    a.extend(["--config".to_string(), conf.to_string()]);
    if cli.noconfirm {
        a.push("--noconfirm".into());
    }
    Ok(a)
}

/// `pacman -Q` argv for the installed versions of `names` (for the selector).
pub fn installed_query_args(cli: &Cli, names: &[String]) -> Vec<String> {
    let mut a = vec!["-Q".to_string(), "--color".into(), "never".into()];
    a.extend(only_flags(&cli.passthrough, &["--config", "--dbpath", "--root", "-b", "-r"]));
    a.push("--".into());
    a.extend(names.iter().cloned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cli(a: &[&str]) -> Cli {
        parse_cli(a.iter().map(|s| s.to_string())).unwrap()
    }
    fn sync(refresh: u8, upgrade: bool) -> Op {
        Op::Sync { refresh, upgrade, download_only: false }
    }

    #[test]
    fn operations() {
        assert_eq!(cli(&["-Syu"]).op, sync(1, true));
        assert_eq!(cli(&["-Syyu"]).op, sync(2, true));
        assert_eq!(cli(&["-Suy"]).op, sync(1, true));
        assert_eq!(cli(&["-S", "-y", "-u"]).op, sync(1, true));
        assert_eq!(cli(&["--sync", "--refresh", "--sysupgrade"]).op, sync(1, true));
        assert_eq!(cli(&["-S", "a"]).op, sync(0, false));
        assert_eq!(cli(&["-Sw", "a"]).op, Op::Sync { refresh: 0, upgrade: false, download_only: true });
        assert_eq!(cli(&["-Rcs", "a"]).op, Op::Remove);
        assert_eq!(cli(&["-Rdd", "a"]).op, Op::Remove);
        let pass: [&[&str]; 7] = [&["-Q"], &["-Ss", "foo"], &["-Si", "foo"], &["-Sc"], &["-S", "--print", "x"], &["-Sh"], &["-U", "x.pkg"]];
        for p in pass {
            assert_eq!(cli(p).op, Op::Pacman, "{p:?}");
        }
        assert_eq!(cli(&["-h"]).op, Op::Help);
        assert_eq!(cli(&["-V"]).op, Op::Version);
        assert!(parse_cli(["-SR".to_string()]).is_err());
        assert!(parse_cli(["foo".to_string()]).is_err());
        assert!(parse_cli(Vec::<String>::new()).is_err());
    }

    #[test]
    fn raw_keeps_everything_but_dry_run() {
        let c = cli(&["-Rns", "--dry-run", "foo", "-b", "/db"]);
        assert_eq!(c.raw, ["-Rns", "foo", "-b", "/db"]);
        assert!(c.dry_run);
    }

    #[test]
    fn flags_reach_both_query_and_install() {
        let c = cli(&["-S", "foo", "bar", "--needed", "--overwrite", "/usr/lib/x/*", "--ignore", "a,b", "--ignore=c", "--noconfirm", "-dd"]);
        assert_eq!(c.targets, ["foo", "bar"]);
        assert_eq!(c.ignore, ["a", "b", "c"]);
        let q = query_args(&c).unwrap();
        let i = install_args(&c).unwrap();
        for args in [&q, &i] {
            assert!(args.contains(&"--needed".to_string()));
            assert!(args.contains(&"-dd".to_string()));
            assert!(args.windows(2).any(|w| w == ["--overwrite", "/usr/lib/x/*"]), "{args:?}");
            assert!(args.contains(&"--ignore=a,b,c".to_string()));
            let dd = args.iter().position(|a| a == "--").unwrap();
            assert_eq!(&args[dd + 1..], ["foo", "bar"], "targets come last, after --");
        }
        assert!(q.contains(&"--print".to_string()) && q.contains(&PRINT_FORMAT.to_string()));
        assert!(i.contains(&"--noconfirm".to_string()) && !i.contains(&"--print".to_string()));
        assert!(!i.contains(&"--color".to_string()), "pacman decides colors for the install");
    }

    #[test]
    fn query_forces_no_color_without_duplicates() {
        let q = query_args(&cli(&["-Su", "--color", "always"])).unwrap();
        assert_eq!(q.iter().filter(|a| *a == "--color").count(), 1);
        assert!(q.windows(2).any(|w| w == ["--color", "never"]));
        let i = install_args(&cli(&["-Su", "--color=always"])).unwrap();
        assert!(i.contains(&"--color=always".to_string()));
    }

    #[test]
    fn refresh_replaces_config_and_counts_y() {
        let c = cli(&["-Syyu", "--config", "/etc/pacman.conf", "--noconfirm", "--dbpath", "/db"]);
        let r = refresh_args(&c, "/tmp/x.conf").unwrap();
        assert_eq!(r[0], "-Syy");
        assert!(r.windows(2).any(|w| w == ["--config", "/tmp/x.conf"]));
        assert!(!r.contains(&"/etc/pacman.conf".to_string()));
        assert!(r.windows(2).any(|w| w == ["--dbpath", "/db"]) && r.contains(&"--noconfirm".to_string()));
    }

    #[test]
    fn download_only_args_keeps_operation_flags_before_targets() {
        let c = cli(&["-Sw", "--needed", "--noconfirm", "foo", "--", "bar"]);
        let a = download_only_args(&c).unwrap();
        assert!(a.contains(&"--downloadonly".to_string()));
        assert!(a.contains(&"--needed".to_string()) && a.contains(&"--noconfirm".to_string()));
        let sep = a.iter().position(|arg| arg == "--").unwrap();
        assert!(a[..sep].contains(&"--downloadonly".to_string()));
        assert_eq!(&a[sep + 1..], ["foo", "bar"]);
    }

    #[test]
    fn value_flags_short_long_and_equals() {
        let c = cli(&["-S", "-b", "/db1", "-r/root", "--cachedir=/c", "x"]);
        assert_eq!(c.passthrough, ["-b", "/db1", "-r", "/root", "--cachedir=/c"]);
        assert_eq!(flag_value(&c.passthrough, "--cachedir"), Some("/c"));
        assert_eq!(flag_value(&c.passthrough, "--config"), None);
        let q = installed_query_args(&c, &["x".into()]);
        assert_eq!(q, ["-Q", "--color", "never", "-b", "/db1", "-r", "/root", "--", "x"]);
        assert!(parse_cli(["-S".to_string(), "--config".to_string()]).is_err());
    }

    #[test]
    fn hostile_target_stays_one_argv_entry() {
        let c = cli(&["-S", "--", "foo; rm -rf /", "$(id)", "--noconfirm"]);
        assert_eq!(c.targets, ["foo; rm -rf /", "$(id)", "--noconfirm"]);
        assert!(!c.noconfirm, "after -- everything is a target");
        let q = query_args(&c).unwrap();
        assert!(q.contains(&"foo; rm -rf /".to_string()));
    }
}
