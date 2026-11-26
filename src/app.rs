//! Orchestration: refresh → query → select → plan → download → install → AUR.
//!
//! Invariants kept from the C version:
//! - pacman is never started for the install if any download failed;
//! - the AUR step only runs after a successful (or empty) repository upgrade;
//! - the exit status is non-zero if any stage failed.
use crate::aur;
use crate::conf::{expansion_arch, parse_conf, parse_options, strip_xfer, wildcard_match, Repo};
use crate::config::{parse_config, Config};
use crate::driver::{write_private, Aria2Runner};
use crate::fmt::{human, strip_ansi};
use crate::pacman::{download_only_args, flag_value, install_args, installed_query_args, query_args, refresh_args, Cli, Op};
use crate::plan::{labels, plan, Plan, PlanCfg, Task};
use crate::render::{self, Dashboard};
use crate::rows::{parse_rows_checked, Pkg};
use crate::sched::{run_all, PkgResult};
use crate::select::{Item, Selector, Step};
use crate::sys;
use crate::urls::{resolve, Source};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const HELP: &str = "\
pacz: pacman with parallel, batched, mirror-aware downloads through aria2-next

usage:  sudo pacz -S [options] <packages>   install packages
        sudo pacz -Sy[y]                    refresh the package databases
        sudo pacz -Su[y] [options]          upgrade: repositories, then AUR (paru)
        sudo pacz -Sw [options] <packages>  download only
        sudo pacz -R[cnsu...] <packages>    remove (handed to pacman)
        pacz -Q..., -Ss, -Si, -U, -F, ...   handed to pacman unchanged

options: pacman's options are passed through (--needed, --overwrite, --ignore, ...)
         --noconfirm   no selector and no prompts (also forwarded to paru, see aur_noconfirm)
         --dry-run     show the download plan; changes nothing, needs no root

selector: numbers/ranges toggle ignore (1 3 5-7), s/n sort by size/name,
          /text search, c clear, Enter continue, q quit
config:   /etc/pacz.conf (override with PACZ_CONFIG)
";

pub struct Logger(Option<Mutex<fs::File>>);

impl Logger {
    pub fn open(path: Option<&Path>) -> Result<Self, String> {
        match path {
            None => Ok(Self(None)),
            Some(p) => fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map(|f| Self(Some(Mutex::new(f))))
                .map_err(|e| format!("cannot open log file {}: {e}", p.display())),
        }
    }

    pub fn log(&self, msg: &str) {
        if let Some(f) = &self.0 {
            let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{t} {msg}");
            }
        }
    }
}

/// Everything resolved once at startup.
pub struct Env {
    pub cfg: Config,
    pub pacman_conf: PathBuf,
    pub cache_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub repos: Vec<Repo>,
    /// stdout is a terminal: live dashboard, selector, cursor codes.
    pub tty: bool,
    pub color: bool,
    pub sudo_user: Option<String>,
    pub log: Logger,
}

fn default_staging(cache: &Path) -> PathBuf {
    cache.parent().map(|p| p.join("pacz-staging")).unwrap_or_else(|| cache.join(".pacz-staging"))
}

/// Include reader with glob support in the file name (`/etc/pacman.d/*.conf`).
pub fn read_include(p: &str) -> Option<String> {
    if !p.contains(['*', '?']) {
        return fs::read_to_string(p).ok();
    }
    let path = Path::new(p);
    let (dir, pat) = (path.parent()?, path.file_name()?.to_str()?);
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|f| f.file_name().and_then(|n| n.to_str()).is_some_and(|n| wildcard_match(pat, n)))
        .collect();
    files.sort();
    Some(files.iter().filter_map(|f| fs::read_to_string(f).ok()).collect::<Vec<_>>().join("\n"))
}

impl Env {
    pub fn load(cli: &Cli, cfg: Config, tty: bool, color: bool, sudo_user: Option<String>) -> Result<Env, String> {
        let pacman_conf = PathBuf::from(flag_value(&cli.passthrough, "--config").unwrap_or("/etc/pacman.conf"));
        let text = fs::read_to_string(&pacman_conf).map_err(|e| format!("cannot read {}: {e}", pacman_conf.display()))?;
        let opts = parse_options(&text);
        let cache_dir = flag_value(&cli.passthrough, "--cachedir")
            .map(PathBuf::from)
            .or_else(|| cfg.cache_dir.clone())
            .or_else(|| opts.cache_dirs.first().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/var/cache/pacman/pkg"));
        let staging_dir = cfg.staging_dir.clone().unwrap_or_else(|| default_staging(&cache_dir));
        let arch = expansion_arch(&opts, std::env::consts::ARCH);
        let repos = parse_conf(&text, &arch, &read_include, cfg.mirrors);
        let log = Logger::open(cfg.log_file.as_deref())?;
        Ok(Env { cfg, pacman_conf, cache_dir, staging_dir, repos, tty, color, sudo_user, log })
    }
}

fn heading(env: &Env, msg: &str) {
    println!("{}", render::heading(msg, env.color));
}

fn error(msg: &str) {
    eprintln!("{} {msg}", render::paint("error:".into(), "1;31", sys::isatty(2)));
}

fn warn(msg: &str) {
    eprintln!("{} {msg}", render::paint("warning:".into(), "1;33", sys::isatty(2)));
}

fn merge(into: &mut Vec<String>, add: Vec<String>) {
    for a in add {
        if !into.contains(&a) {
            into.push(a);
        }
    }
}

/// Reads one line from stdin with Ctrl-C working normally. `None` on EOF/error.
fn prompt(msg: &str) -> Option<String> {
    print!("{msg}");
    let _ = io::stdout().flush();
    sys::default_sigint();
    let mut s = String::new();
    let r = io::stdin().read_line(&mut s);
    sys::catch_signals();
    match r {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s),
    }
}

fn confirm(msg: &str) -> bool {
    match prompt(&format!(":: {msg} [Y/n] ")) {
        Some(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes"),
        None => false,
    }
}

/// Runs the selector. `Some(ignored names)` to continue, `None` if the user quit.
fn select(env: &Env, title: &str, items: Vec<Item>) -> Option<Vec<String>> {
    let mut sel = Selector::new(items);
    let mut show = true;
    loop {
        if show {
            for l in sel.render(title, env.color) {
                println!("{l}");
            }
        }
        let line = prompt("> ")?;
        match sel.apply(&line) {
            Step::Done => return Some(sel.ignored()),
            Step::Quit => return None,
            Step::Again => show = true,
            Step::Msg(m) => {
                println!("{m}");
                show = false;
            }
        }
    }
}

fn exec_pacman(bin: &str, args: &[String]) -> u8 {
    let e = Command::new(bin).args(args).exec();
    error(&format!("cannot run {bin}: {e}"));
    1
}

fn load_config() -> Result<Config, String> {
    let path = std::env::var_os("PACZ_CONFIG").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/etc/pacz.conf"));
    match fs::read_to_string(&path) {
        Ok(t) => parse_config(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Entry point for everything except the `plan` debug subcommand. Returns the exit code.
pub fn main(cli: Cli) -> u8 {
    match cli.op {
        Op::Help => {
            print!("{HELP}");
            return 0;
        }
        Op::Version => {
            println!("pacz {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        _ => {}
    }
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error(&e);
            return 2;
        }
    };
    match cli.op {
        Op::Remove => {
            if !sys::is_root() {
                error("removing packages needs root: run sudo pacz ...");
                return 1;
            }
            exec_pacman(&cfg.pacman_bin, &cli.raw)
        }
        Op::Sync { .. } => {
            if !cli.dry_run && !sys::is_root() {
                error("this operation needs root: run sudo pacz ... (or add --dry-run)");
                return 1;
            }
            let tty = sys::isatty(1);
            let color = tty && std::env::var_os("NO_COLOR").is_none();
            let env = match Env::load(&cli, cfg, tty, color, std::env::var("SUDO_USER").ok()) {
                Ok(e) => e,
                Err(e) => {
                    error(&e);
                    return 1;
                }
            };
            sys::catch_signals();
            if tty {
                let prev = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    eprint!("{}", render::SHOW_CURSOR);
                    prev(info);
                }));
            }
            run_sync(&env, cli)
        }
        _ => exec_pacman(&cfg.pacman_bin, &cli.raw),
    }
}

/// `-S`, `-Sy`, `-Su`, `-Syu`, `-Sw`.
pub fn run_sync(env: &Env, mut cli: Cli) -> u8 {
    let Op::Sync { refresh, upgrade, download_only } = cli.op else { return 2 };
    if flag_value(&cli.passthrough, "--sysroot").is_some() {
        error("--sysroot is not supported by pacz; use pacman directly");
        return 2;
    }
    env.log.log(&format!("start: pacz {}", cli.raw.join(" ")));

    if refresh > 0 && !cli.dry_run {
        if let Err(e) = refresh_dbs(env, &cli) {
            error(&e);
            return 1;
        }
    }
    if !upgrade && cli.targets.is_empty() {
        return 0; // plain -Sy
    }

    let mut pkgs = match query(env, &cli) {
        Ok(p) => p,
        Err(e) => {
            error(&e);
            return 1;
        }
    };
    if !cli.dry_run && !pkgs.is_empty() {
        if upgrade && !cli.noconfirm {
            let items = repo_items(env, &cli, &pkgs);
            match select(env, "Repository updates", items) {
                None => {
                    heading(env, "Aborted");
                    return 1;
                }
                Some(ignored) if !ignored.is_empty() => {
                    merge(&mut cli.ignore, ignored);
                    // Ignoring packages can change what else gets pulled in: ask pacman again.
                    pkgs = match query(env, &cli) {
                        Ok(p) => p,
                        Err(e) => {
                            error(&e);
                            return 1;
                        }
                    };
                    heading(env, &format!("{} packages after ignoring {}", pkgs.len(), cli.ignore.join(", ")));
                }
                Some(_) => {}
            }
        } else {
            print_details(env, &pkgs);
            if !cli.noconfirm && !confirm("Proceed with download?") {
                return 1;
            }
        }
    }

    let repo_ok = if pkgs.is_empty() && upgrade {
        heading(env, "No repository updates");
        if cli.dry_run {
            return 0;
        }
        true
    } else {
        let plan_cfg = PlanCfg { small_bytes: env.cfg.small_bytes, batch_max: env.cfg.batch_max, batch_min: env.cfg.batch_min };
        let p = plan(pkgs, &plan_cfg);
        if cli.dry_run {
            for l in describe_plan(&p, &env.repos) {
                println!("{l}");
            }
            return 0;
        }
        if !p.no_sha.is_empty() {
            let names: Vec<&str> = p.no_sha.iter().map(|p| p.name.as_str()).collect();
            if !env.cfg.fallback_to_pacman {
                error(&format!(
                    "no sha256 in the sync database for: {}; refusing unverified downloads \
                     (set fallback_to_pacman = true to let pacman fetch and verify them)",
                    names.join(", ")
                ));
                return 1;
            }
            if download_only {
                warn(&format!("no sha256 in the sync database for: {}; pacman will download and verify these", names.join(", ")));
            } else {
                warn(&format!("no sha256 in the sync database for: {}; pacman will download these itself", names.join(", ")));
            }
        }
        if !p.tasks.is_empty() {
            match download(env, &p.tasks, p.cached.len()) {
                Outcome::Ok => {}
                Outcome::Failed => return 1,
                Outcome::Interrupted => return 130,
            }
        } else if !p.cached.is_empty() {
            heading(env, &format!("All {} packages are already in the cache", p.cached.len()));
        }
        if download_only {
            if !p.no_sha.is_empty() && !download_missing_with_pacman(env, &cli) {
                return 1;
            }
            heading(env, "Downloads complete; nothing installed (--downloadonly)");
            return 0;
        }
        install(env, &cli)
    };
    if !repo_ok {
        return 1;
    }
    if upgrade && env.cfg.aur && !cli.dry_run && !aur_step(env, &cli) {
        return 1;
    }
    0
}

fn refresh_dbs(env: &Env, cli: &Cli) -> Result<(), String> {
    let text = fs::read_to_string(&env.pacman_conf).map_err(|e| format!("cannot read {}: {e}", env.pacman_conf.display()))?;
    let tmp = env.cfg.tmp_dir.join(format!("pacz-pacman-{}.conf", std::process::id()));
    write_private(&tmp, &strip_xfer(&text)).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    let status = refresh_args(cli, &tmp.to_string_lossy()).and_then(|a| {
        Command::new(&env.cfg.pacman_bin).args(a).status().map_err(|e| format!("cannot run {}: {e}", env.cfg.pacman_bin))
    });
    let _ = fs::remove_file(&tmp);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("database refresh failed ({s})")),
        Err(e) => Err(e),
    }
}

/// Runs the `--print` query. Warnings are shown; any `error:` line or a non-zero exit
/// is fatal, with pacman's text.
fn query(env: &Env, cli: &Cli) -> Result<Vec<Pkg>, String> {
    let args = query_args(cli)?;
    let out = Command::new(&env.cfg.pacman_bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", env.cfg.pacman_bin))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut has_error = false;
    for line in stderr.lines() {
        let plain = strip_ansi(line);
        let t = plain.trim_start();
        if t.starts_with("error:") {
            has_error = true;
        } else if let Some(w) = t.strip_prefix("warning:") {
            warn(w.trim());
        }
    }
    if has_error || !out.status.success() {
        let mut msg = format!("pacman could not resolve the transaction ({})", out.status);
        let text = strip_ansi(stderr.trim());
        if !text.is_empty() {
            msg.push('\n');
            msg.push_str(&text);
        }
        if text.contains("conflict") {
            msg.push_str("\n(pacz cannot answer pacman's conflict questions; run this operation with plain pacman)");
        }
        return Err(msg);
    }
    let pkgs = parse_rows_checked(&stdout).map_err(|e| format!("pacman returned unexpected --print output: {e}"))?;
    env.log.log(&format!("query: {} packages", pkgs.len()));
    Ok(pkgs)
}

fn installed_versions(env: &Env, cli: &Cli, names: &[String]) -> HashMap<String, String> {
    let Ok(out) = Command::new(&env.cfg.pacman_bin).args(installed_query_args(cli, names)).stdin(Stdio::null()).output() else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let versions = text
        .lines()
        .filter_map(|l| {
            let mut t = l.split_whitespace();
            Some((t.next()?.to_owned(), t.next()?.to_owned()))
        })
        .collect();
    versions
}

fn repo_items(env: &Env, cli: &Cli, pkgs: &[Pkg]) -> Vec<Item> {
    let names: Vec<String> = pkgs.iter().map(|p| p.name.clone()).collect();
    let installed = installed_versions(env, cli, &names);
    pkgs.iter()
        .map(|p| {
            let old = installed.get(&p.name).map_or("(new)", String::as_str);
            let cached = if p.is_cached() { "  (cached)" } else { "" };
            Item { name: p.name.clone(), detail: format!("{old} -> {}{cached}", p.version), size: Some(p.size) }
        })
        .collect()
}

fn print_details(env: &Env, pkgs: &[Pkg]) {
    let total: u64 = pkgs.iter().map(|p| p.size).sum();
    heading(env, &format!("Packages ({})  download size: {}", pkgs.len(), human(total)));
    let nw = pkgs.iter().map(|p| p.name.chars().count()).max().unwrap_or(0).min(40);
    for p in pkgs {
        let size = if p.is_cached() { "cached".to_string() } else { human(p.size) };
        println!("  {}  {:<24}  {size:>9}", render::fit(&p.name, nw), p.version);
    }
}

/// Human-readable plan, for `--dry-run` and `pacz plan`.
pub fn describe_plan(p: &Plan, repos: &[Repo]) -> Vec<String> {
    let total: u64 = p.tasks.iter().map(Task::bytes).sum();
    let mut out = vec![format!(
        ":: {} tasks ({}), {} cached, {} without sha256",
        p.tasks.len(),
        human(total),
        p.cached.len(),
        p.no_sha.len()
    )];
    for (t, label) in p.tasks.iter().zip(labels(&p.tasks)) {
        let kind = if matches!(t, Task::Batch(_)) { "batch" } else { "single" };
        let mirrors = t.pkgs().first().map_or(0, |pk| match resolve(repos, pk) {
            Source::Urls(u) => u.len(),
            _ => 0,
        });
        out.push(format!("  {kind:<7} {label:<30} {:>9}  mirrors={mirrors}", human(t.bytes())));
    }
    out
}

enum Outcome {
    Ok,
    Failed,
    Interrupted,
}

fn redraw(env: &Env, dash: &Dashboard, drawn: usize) -> usize {
    let (w, h) = sys::term_size().unwrap_or((80, 24));
    let frame = dash.frame(w, env.color, h.saturating_sub(4).clamp(1, 20));
    let mut s = render::rewind(drawn);
    s.push_str(render::CLEAR_BELOW);
    for l in &frame {
        s.push_str(l);
        s.push('\n');
    }
    print!("{s}");
    let _ = io::stdout().flush();
    frame.len()
}

fn download(env: &Env, tasks: &[Task], cached: usize) -> Outcome {
    let total: u64 = tasks.iter().map(Task::bytes).sum();
    let count: usize = tasks.iter().map(|t| t.pkgs().len()).sum();
    for dir in [Some(&env.staging_dir), env.cfg.state_dir.as_ref()].into_iter().flatten() {
        if let Err(e) = fs::create_dir_all(dir) {
            error(&format!("cannot create {}: {e}", dir.display()));
            return Outcome::Failed;
        }
    }
    let cached_note = if cached > 0 { format!(", {cached} already cached") } else { String::new() };
    heading(env, &format!("Downloading {count} packages ({}) in {} aria2 runs{cached_note}", human(total), tasks.len()));
    env.log.log(&format!("download: {count} packages, {} tasks", tasks.len()));

    let dash = Dashboard::new();
    for (i, (t, l)) in tasks.iter().zip(labels(tasks)).enumerate() {
        dash.add(i, &l, t.bytes());
    }
    let mut runner =
        Aria2Runner::new(env.cfg.aria2_bin.clone(), env.cache_dir.clone(), env.staging_dir.clone(), env.cfg.tmp_dir.clone(), env.repos.clone());
    runner.state_dir = env.cfg.state_dir.clone();
    runner.check_integrity = env.cfg.check_integrity;
    runner.extra_args = env.cfg.aria2_args.clone();
    if env.tty {
        runner.on_progress = Some(dash.progress_fn());
    }
    let on_done = |id: usize, res: &[PkgResult]| {
        dash.finish(id, res.iter().all(|(_, r)| r.is_ok()));
        for (name, r) in res {
            let line = match r {
                Ok(()) => format!("completed: {name}"),
                Err(e) => format!("failed: {name}: {e}"),
            };
            env.log.log(&line);
            if !env.tty {
                println!("{line}");
            }
        }
    };
    let workers = env.cfg.workers.clamp(1, tasks.len().max(1));
    let (results, interrupted) = std::thread::scope(|s| {
        let handle = s.spawn(|| run_all(tasks, workers, &runner, &on_done));
        let (mut drawn, mut interrupted) = (0usize, false);
        if env.tty {
            print!("{}", render::HIDE_CURSOR);
        }
        loop {
            let finished = handle.is_finished();
            if !interrupted && sys::interrupted() {
                interrupted = true;
                runner.cancel();
            }
            if env.tty {
                drawn = redraw(env, &dash, drawn);
            }
            if finished {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        if env.tty {
            print!("{}", render::SHOW_CURSOR);
            let _ = io::stdout().flush();
        }
        let results: Vec<PkgResult> = handle.join().unwrap_or_else(|_| {
            tasks.iter().flat_map(|t| t.pkgs()).map(|p| (p.name.clone(), Err("internal error".to_string()))).collect()
        });
        (results, interrupted)
    });

    if interrupted {
        error("interrupted: nothing was installed; partial downloads are kept and resume next time");
        return Outcome::Interrupted;
    }
    let failed: Vec<&PkgResult> = results.iter().filter(|(_, r)| r.is_err()).collect();
    if !failed.is_empty() {
        error(&format!("{} of {count} packages failed to download; not starting pacman:", failed.len()));
        for (name, r) in failed {
            if let Err(e) = r {
                eprintln!("  {name}: {e}");
            }
        }
        return Outcome::Failed;
    }
    heading(env, &format!("{count} packages downloaded"));
    Outcome::Ok
}

fn install(env: &Env, cli: &Cli) -> bool {
    let args = match install_args(cli) {
        Ok(a) => a,
        Err(e) => {
            error(&e);
            return false;
        }
    };
    env.log.log(&format!("install: pacman {}", args.join(" ")));
    match Command::new(&env.cfg.pacman_bin).args(args).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            error(&format!("pacman failed ({s})"));
            false
        }
        Err(e) => {
            error(&format!("cannot run {}: {e}", env.cfg.pacman_bin));
            false
        }
    }
}

fn download_missing_with_pacman(env: &Env, cli: &Cli) -> bool {
    let args = match download_only_args(cli) {
        Ok(a) => a,
        Err(e) => {
            error(&e);
            return false;
        }
    };
    env.log.log(&format!("download fallback: pacman {}", args.join(" ")));
    match Command::new(&env.cfg.pacman_bin).args(args).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            error(&format!("pacman download fallback failed ({s})"));
            false
        }
        Err(e) => {
            error(&format!("cannot run {}: {e}", env.cfg.pacman_bin));
            false
        }
    }
}

/// paru as the invoking user: PATH/HOME/USER/LOGNAME set, SUDO_* removed, cwd = home,
/// supplementary groups + gid + uid dropped.
fn paru(env: &Env, user: &sys::User, args: &[String]) -> Command {
    let mut c = Command::new(&env.cfg.paru_bin);
    c.args(args).current_dir(&user.home);
    let keys = std::env::vars_os().filter_map(|(k, _)| k.into_string().ok());
    let (set, remove) = aur::user_env(&user.name, &user.home.to_string_lossy(), keys);
    for k in remove {
        c.env_remove(k);
    }
    for (k, v) in set {
        c.env(k, v);
    }
    sys::run_as(&mut c, user);
    c
}

fn aur_step(env: &Env, cli: &Cli) -> bool {
    let name = match aur::sudo_user(env.sudo_user.as_deref()) {
        Ok(n) => n.to_owned(),
        Err(e) => {
            error(&e);
            return false;
        }
    };
    let user = match sys::lookup_user(&name) {
        Ok(u) => u,
        Err(e) => {
            error(&e);
            return false;
        }
    };
    heading(env, "Checking AUR updates");
    let out = match paru(env, &user, &aur::qua_args()).stdin(Stdio::null()).output() {
        Ok(o) => o,
        Err(e) => {
            error(&format!("cannot run {}: {e}", env.cfg.paru_bin));
            return false;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let updates = aur::parse_qua(&stdout);
    if updates.is_empty() {
        // `paru -Qua` exits 1 when there is nothing to update; only real errors fail.
        if stderr.lines().any(|l| strip_ansi(l).trim_start().starts_with("error")) {
            error(&format!("paru -Qua failed:\n{}", stderr.trim()));
            return false;
        }
        heading(env, "No AUR updates");
        return true;
    }
    let mut ignore = cli.ignore.clone();
    if cli.noconfirm {
        if env.cfg.aur_noconfirm {
            warn("--noconfirm is passed to paru: PKGBUILDs and diffs will not be shown for review");
        }
    } else {
        let items = updates.iter().map(|u| Item { name: u.name.clone(), detail: format!("{} -> {}", u.old, u.new), size: None }).collect();
        match select(env, "AUR updates", items) {
            None => {
                heading(env, "AUR updates skipped");
                return false;
            }
            Some(ign) => merge(&mut ignore, ign),
        }
    }
    if updates.iter().all(|u| ignore.contains(&u.name)) {
        heading(env, "All AUR updates ignored");
        return true;
    }
    let args = aur::sua_args(cli.noconfirm && env.cfg.aur_noconfirm, &ignore);
    env.log.log(&format!("aur: paru {} (as {name})", args.join(" ")));
    match paru(env, &user, &args).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            error(&format!("paru failed ({s})"));
            false
        }
        Err(e) => {
            error(&format!("cannot run {}: {e}", env.cfg.paru_bin));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end runs against a stub `pacman` and a stub `aria2-next` (no root, no network).
    use super::*;
    use crate::pacman::parse_cli;
    use std::os::unix::fs::PermissionsExt;

    const PACMAN: &str = r#"#!/bin/sh
echo "$*" >> "@ROOT@/pacman.log"
for a in "$@"; do
  if [ "$a" = "--print" ]; then cat "@ROOT@/rows.txt"; exit 0; fi
done
exit 0
"#;

    const ARIA2: &str = r#"#!/bin/sh
echo "$*" >> "@ROOT@/aria2.log"
while [ $# -gt 0 ]; do case "$1" in --input-file) IN="$2"; shift;; --dir) DIR="$2"; shift;; esac; shift; done
rc=0
for out in $(grep '^  out=' "$IN" | sed 's/^  out=//'); do
  case "$out" in
    FAIL*) rc=1; echo "abc123|ERR |   0B/s|$DIR/$out";;
    *) echo data > "$DIR/$out"; echo "01/01 00:00:00 [NOTICE] Download complete: $DIR/$out";;
  esac
done
exit $rc
"#;

    struct Fixture {
        root: PathBuf,
        cfg: Config,
    }

    fn write_exe(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 150 tiny + 3 big packages, plus `extra` tiny ones.
    fn fixture(tag: &str, extra: &[&str]) -> Fixture {
        let root = std::env::temp_dir().join(format!("pacz-e2e-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in ["cache", "tmp"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let r = root.display().to_string();
        write_exe(&root.join("pacman"), &PACMAN.replace("@ROOT@", &r));
        write_exe(&root.join("aria2-next"), &ARIA2.replace("@ROOT@", &r));
        let sha = "a".repeat(64);
        let mut rows = String::new();
        let mut row = |name: &str, size: u64| {
            let file = format!("{name}-1-1-x86_64.pkg.tar.zst");
            rows.push_str(&format!("{name} 1-1 {size} {file} core https://m.example/core/os/x86_64/{file} {sha}\n"));
        };
        for i in 0..150u64 {
            row(&format!("tiny{i}"), 1000 + i);
        }
        for i in 0..3u64 {
            row(&format!("big{i}"), (100 + i) << 20);
        }
        for name in extra {
            row(name, 1000);
        }
        fs::write(root.join("rows.txt"), rows).unwrap();
        fs::write(
            root.join("pacman.conf"),
            "[options]\nXferCommand = /bin/false %u\n[core]\nServer = https://m.example/$repo/os/$arch\n",
        )
        .unwrap();
        let cfg = Config {
            pacman_bin: format!("{r}/pacman"),
            aria2_bin: format!("{r}/aria2-next"),
            tmp_dir: root.join("tmp"),
            state_dir: None,
            aur: false,
            ..Config::default()
        };
        Fixture { root, cfg }
    }

    fn run(f: &Fixture, args: &[&str]) -> u8 {
        let mut a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        a.extend([
            "--config".to_string(),
            f.root.join("pacman.conf").display().to_string(),
            "--cachedir".to_string(),
            f.root.join("cache").display().to_string(),
        ]);
        let cli = parse_cli(a).unwrap();
        let env = Env::load(&cli, f.cfg.clone(), false, false, None).unwrap();
        run_sync(&env, cli)
    }

    fn lines(p: PathBuf) -> Vec<String> {
        fs::read_to_string(p).unwrap_or_default().lines().map(String::from).collect()
    }

    fn count(p: PathBuf) -> usize {
        fs::read_dir(p).map(|d| d.count()).unwrap_or(0)
    }

    #[test]
    fn upgrade_batches_downloads_and_forwards_flags_to_both_calls() {
        let f = fixture("ok", &[]);
        assert_eq!(run(&f, &["-Su", "--needed", "--ignore", "foo", "--noconfirm"]), 0);
        assert_eq!(lines(f.root.join("aria2.log")).len(), 3 + 4, "3 singles + ceil(150/40) batches");
        assert_eq!(count(f.root.join("cache")), 153);
        assert_eq!(count(f.root.join("pacz-staging")), 0, "staging emptied");
        assert_eq!(count(f.root.join("tmp")), 0, "no temp files left");
        let pac = lines(f.root.join("pacman.log"));
        assert_eq!(pac.len(), 2, "query + install, no refresh: {pac:?}");
        let (q, i) = (&pac[0], &pac[1]);
        assert!(q.contains("--print") && !i.contains("--print"));
        for l in [q, i] {
            assert!(l.contains("--needed") && l.contains("--ignore=foo"), "{l}");
        }
        assert!(i.starts_with("-Su ") && i.contains("--noconfirm"));
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn one_failed_download_blocks_the_install() {
        let f = fixture("fail", &["FAILpkg"]);
        assert_eq!(run(&f, &["-Su", "--noconfirm"]), 1);
        let pac = lines(f.root.join("pacman.log"));
        assert_eq!(pac.len(), 1, "only the query ran: {pac:?}");
        assert!(!f.root.join("cache/FAILpkg-1-1-x86_64.pkg.tar.zst").exists());
        assert!(f.root.join("cache/big0-1-1-x86_64.pkg.tar.zst").exists());
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn refresh_uses_a_temporary_conf_and_removes_it() {
        let f = fixture("refresh", &[]);
        assert_eq!(run(&f, &["-Syu", "--noconfirm"]), 0);
        let pac = lines(f.root.join("pacman.log"));
        assert_eq!(pac.len(), 3, "{pac:?}");
        assert!(pac[0].starts_with("-Sy ") && pac[0].contains("pacz-pacman-"), "{}", pac[0]);
        assert!(!pac[0].contains("/pacman.conf"), "the original conf (with XferCommand) is not used");
        assert!(!pac[1].contains("-Sy") && !pac[2].contains("-Sy"), "no second refresh");
        assert_eq!(count(f.root.join("tmp")), 0);
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn plain_refresh_only_refreshes() {
        let f = fixture("sy", &[]);
        assert_eq!(run(&f, &["-Sy"]), 0);
        assert_eq!(lines(f.root.join("pacman.log")).len(), 1);
        assert!(!f.root.join("aria2.log").exists());
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn dry_run_only_queries() {
        let f = fixture("dry", &[]);
        assert_eq!(run(&f, &["-Syu", "--dry-run"]), 0);
        let pac = lines(f.root.join("pacman.log"));
        assert_eq!(pac.len(), 1);
        assert!(pac[0].contains("--print"));
        assert!(!f.root.join("aria2.log").exists());
        assert_eq!(count(f.root.join("cache")), 0);
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn download_only_does_not_install() {
        let f = fixture("w", &[]);
        assert_eq!(run(&f, &["-Sw", "--noconfirm", "tiny1", "big0"]), 0);
        let pac = lines(f.root.join("pacman.log"));
        assert_eq!(pac.len(), 1, "{pac:?}");
        assert!(pac[0].ends_with("-- tiny1 big0"), "{}", pac[0]);
        assert_eq!(count(f.root.join("cache")), 153, "stub pacman returns all rows");
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn download_only_falls_back_to_pacman_for_packages_without_sha256() {
        let f = fixture("w-nosha", &[]);
        let rows = fs::read_to_string(f.root.join("rows.txt")).unwrap();
        let selected: Vec<String> = rows.lines().take(2).map(String::from).collect();
        let mut first: Vec<String> = selected[0].split_whitespace().map(String::from).collect();
        first.pop();
        fs::write(f.root.join("rows.txt"), format!("{}\n{}\n", selected[1], first.join(" "))).unwrap();

        assert_eq!(run(&f, &["-Sw", "--noconfirm", "tiny0", "tiny1"]), 0);
        let pac = lines(f.root.join("pacman.log"));
        assert_eq!(pac.len(), 2, "query, then pacman's verified download fallback: {pac:?}");
        assert!(pac[1].contains("--downloadonly") && pac[1].contains("--noconfirm"), "{}", pac[1]);
        assert!(pac[1].ends_with("-- tiny0 tiny1"), "{}", pac[1]);
        assert!(f.root.join("cache/tiny1-1-1-x86_64.pkg.tar.zst").exists(), "the sha256 package was downloaded by aria2 first");
        let _ = fs::remove_dir_all(&f.root);
    }

    #[test]
    fn include_globs_are_read_in_order() {
        let dir = std::env::temp_dir().join(format!("pacz-glob-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.conf"), "Server = https://b/$repo").unwrap();
        fs::write(dir.join("a.conf"), "Server = https://a/$repo").unwrap();
        fs::write(dir.join("c.txt"), "Server = https://c/$repo").unwrap();
        let body = read_include(&format!("{}/*.conf", dir.display())).unwrap();
        assert_eq!(body, "Server = https://a/$repo\nServer = https://b/$repo");
        let _ = fs::remove_dir_all(&dir);
    }
}
