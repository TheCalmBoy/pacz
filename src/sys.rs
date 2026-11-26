//! The few libc calls pacz needs: root check, terminal size, signals, user lookup and
//! the privilege drop for the AUR step. Everything unsafe lives here.
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

pub fn isatty(fd: i32) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

/// (columns, rows) of the terminal on stdout.
pub fn term_size() -> Option<(usize, usize)> {
    // SAFETY: winsize is plain data; TIOCGWINSZ fills it in.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws as *mut libc::winsize) };
    (r == 0 && ws.ws_col > 0).then_some((ws.ws_col as usize, ws.ws_row.max(1) as usize))
}

/// SIGINT/SIGTERM only set a flag, checked by the download loop. pacz therefore stays
/// alive while pacman/paru handle their own Ctrl-C; exec'd children get default
/// dispositions back automatically.
pub fn catch_signals() {
    let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only stores to an atomic (async-signal-safe).
    unsafe {
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

/// Default Ctrl-C behaviour, used while waiting for keyboard input (a blocked read would
/// otherwise ignore Ctrl-C until Enter).
pub fn default_sigint() {
    // SAFETY: restoring the default disposition.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

pub fn terminate(pid: u32) {
    // SAFETY: sending a signal has no memory-safety implications.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    /// Supplementary groups, as initgroups would set them.
    pub groups: Vec<u32>,
}

pub fn lookup_user(name: &str) -> Result<User, String> {
    let cname = CString::new(name).map_err(|_| format!("invalid user name {name:?}"))?;
    // SAFETY: passwd is plain data filled by getpwnam_r; buf outlives every use of it.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf: Vec<libc::c_char> = vec![0; 1 << 16];
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) };
    if rc != 0 || res.is_null() || pwd.pw_dir.is_null() {
        return Err(format!("unknown user {name}"));
    }
    let home = PathBuf::from(unsafe { CStr::from_ptr(pwd.pw_dir) }.to_string_lossy().into_owned());
    let (uid, gid) = (pwd.pw_uid, pwd.pw_gid);
    let mut n: libc::c_int = 32;
    loop {
        let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
        let mut count = n;
        // SAFETY: groups has room for `count` entries.
        let r = unsafe { libc::getgrouplist(cname.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
        if r >= 0 {
            groups.truncate(count.max(0) as usize);
            return Ok(User { name: name.to_owned(), uid, gid, home, groups });
        }
        if n >= 1 << 16 {
            return Err(format!("cannot list the groups of {name}"));
        }
        n = if count > n { count } else { n * 2 };
    }
}

/// Makes `cmd` run as `u` (groups, then gid, then uid). Done in one pre_exec hook because
/// std's own `uid()` runs before pre_exec hooks and would make `setgroups` fail.
pub fn run_as(cmd: &mut Command, u: &User) {
    let (uid, gid, groups) = (u.uid, u.gid, u.groups.clone());
    // SAFETY: the hook only makes async-signal-safe syscalls and does not allocate.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            if uid != 0 && libc::setuid(0) == 0 {
                return Err(io::Error::from_raw_os_error(libc::EPERM)); // drop did not stick
            }
            Ok(())
        });
    }
}
