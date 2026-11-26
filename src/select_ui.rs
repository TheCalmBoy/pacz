//! Full-screen selector for interactive terminals. The line-based selector remains in
//! `app` for pipes, redirected input, and terminals where raw mode cannot be enabled.
use crate::fmt::human;
use crate::render::{self, fit, heading, paint};
use crate::select::{Selector, Sort};
use crate::sys;
use std::io::{self, Write};

struct Terminal {
    original: libc::termios,
    screen_active: bool,
}

impl Terminal {
    fn enter() -> io::Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the termios value on success.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful tcgetattr call above initialized this value.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // Keep ISIG enabled so Ctrl-C still reaches pacz's signal handler.
        // SAFETY: raw is a valid termios copied from the current terminal.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let mut terminal = Self {
            original,
            screen_active: true,
        };
        let mut out = io::stdout().lock();
        out.write_all(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?25l")?;
        out.flush()?;
        terminal.screen_active = true;
        Ok(terminal)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.screen_active {
            let mut out = io::stdout().lock();
            let _ = out.write_all(b"\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l");
            let _ = out.flush();
        }
        // SAFETY: original was captured from this terminal by tcgetattr.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    WheelUp,
    WheelDown,
    Enter,
    Backspace,
    Escape,
    CtrlC,
    Char(char),
    Timeout,
    Other,
}

fn read_byte(timeout_ms: i32) -> io::Result<Option<u8>> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: fd points to one initialized pollfd and poll only writes revents.
    let ready = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
    if ready == 0 {
        return Ok(None);
    }
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut byte = 0u8;
    // SAFETY: byte is writable for one byte and stdin is a valid descriptor.
    match unsafe {
        libc::read(
            libc::STDIN_FILENO,
            &mut byte as *mut u8 as *mut libc::c_void,
            1,
        )
    } {
        1 => Ok(Some(byte)),
        0 => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "terminal input closed",
        )),
        _ => Err(io::Error::last_os_error()),
    }
}

fn decode_mouse(sequence: &[u8]) -> Option<Key> {
    let (&end, body) = sequence.split_last()?;
    if end != b'M' && end != b'm' {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?.strip_prefix('<')?;
    let button = text.split(';').next()?.parse::<u16>().ok()?;
    if button & 64 == 0 {
        return None;
    }
    Some(if button & 1 == 0 {
        Key::WheelUp
    } else {
        Key::WheelDown
    })
}

fn escape_key() -> io::Result<Key> {
    let Some(second) = read_byte(40)? else {
        return Ok(Key::Escape);
    };
    if second == b'O' {
        return Ok(match read_byte(40)? {
            Some(b'A') => Key::Up,
            Some(b'B') => Key::Down,
            _ => Key::Other,
        });
    }
    if second != b'[' {
        return Ok(Key::Escape);
    }
    let Some(first) = read_byte(40)? else {
        return Ok(Key::Escape);
    };
    if first == b'M' {
        // Legacy X10 wheel report: button, column, row, each offset by 32.
        let Some(button) = read_byte(40)? else {
            return Ok(Key::Other);
        };
        let _ = read_byte(40)?;
        let _ = read_byte(40)?;
        let button = button.saturating_sub(32);
        if button & 64 != 0 {
            return Ok(if button & 1 == 0 {
                Key::WheelUp
            } else {
                Key::WheelDown
            });
        }
        return Ok(Key::Other);
    }
    let mut sequence = vec![first];
    while sequence.len() < 48 {
        if let Some(&last) = sequence.last() {
            if (0x40..=0x7e).contains(&last) {
                break;
            }
        }
        let Some(next) = read_byte(40)? else {
            return Ok(Key::Escape);
        };
        sequence.push(next);
    }
    if let Some(mouse) = decode_mouse(&sequence) {
        return Ok(mouse);
    }
    Ok(match sequence.last().copied() {
        Some(b'A') => Key::Up,
        Some(b'B') => Key::Down,
        _ if sequence.ends_with(b"5~") => Key::PageUp,
        _ if sequence.ends_with(b"6~") => Key::PageDown,
        _ => Key::Other,
    })
}

fn read_key() -> io::Result<Key> {
    let Some(first) = read_byte(200)? else {
        return Ok(Key::Timeout);
    };
    Ok(match first {
        3 => Key::CtrlC,
        b'\r' | b'\n' => Key::Enter,
        8 | 127 => Key::Backspace,
        27 => return escape_key(),
        b if b.is_ascii() && !b.is_ascii_control() => Key::Char(b as char),
        b if b >= 0xc2 => {
            let width = if b < 0xe0 {
                2
            } else if b < 0xf0 {
                3
            } else {
                4
            };
            let mut bytes = vec![b];
            for _ in 1..width {
                match read_byte(100)? {
                    Some(next) => bytes.push(next),
                    None => break,
                }
            }
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.chars().next())
                .map_or(Key::Other, Key::Char)
        }
        _ => Key::Other,
    })
}

fn visible_match_count(selector: &Selector) -> usize {
    selector.view().len()
}

fn highlighted_name(name: &str, filter: &str, color: bool) -> String {
    let query = filter.to_ascii_lowercase();
    if query.is_empty() {
        return name.to_owned();
    }
    let lower = name.to_ascii_lowercase();
    let Some(start) = lower.find(&query) else {
        return name.to_owned();
    };
    let end = start + query.len();
    format!(
        "{}{}{}",
        &name[..start],
        paint(name[start..end].to_owned(), "1;30;43", color),
        &name[end..]
    )
}

struct MenuState<'a> {
    title: &'a str,
    selected: usize,
    offset: usize,
    width: usize,
    height: usize,
    searching: bool,
    color: bool,
}

fn render_menu(selector: &Selector, state: MenuState<'_>) -> Vec<String> {
    let MenuState {
        title,
        selected,
        offset,
        width,
        height,
        searching,
        color,
    } = state;
    let width = width.saturating_sub(1).max(20);
    let height = height.max(6);
    let body_rows = height.saturating_sub(6).max(1);
    let view = selector.view();
    let sort = match selector.sort {
        Sort::Size => "size, largest first",
        Sort::Name => "name",
    };
    let mut lines = vec![heading(
        &format!(
            "PACZ / {}  ·  {} packages  ·  sorted by {sort}",
            title.to_ascii_uppercase(),
            selector.len()
        ),
        color,
    )];
    let mut controls = "↑/↓ or j/k move  ·  Space ignore  ·  / find  ·  Enter continue  ·  q quit";
    if selector.has_sizes() {
        controls = "s/n sort  ·  ↑/↓ or j/k move  ·  Space ignore  ·  / find  ·  Enter continue  ·  q quit";
    }
    lines.push(format!("  {controls}  ·  mouse wheel scroll"));
    lines.push("─".repeat(width));

    let rank_width = selector.len().to_string().len();
    let available = width.saturating_sub(rank_width + 19);
    let name_width = available.saturating_sub(24).clamp(8, 40);
    let detail_width = available.saturating_sub(name_width).min(32);
    for row in offset..offset.saturating_add(body_rows) {
        if let Some((rank, item, ignored)) = view.get(row) {
            let cursor = if row == selected {
                paint(">".into(), "1;36", color)
            } else {
                " ".into()
            };
            let mark = if *ignored {
                paint("x".into(), "1;31", color)
            } else {
                " ".into()
            };
            let name: String = item.name.chars().take(name_width).collect();
            let name = highlighted_name(&name, &selector.filter, color);
            let prefix = format!("{cursor} {rank:>rank_width$} [{mark}] ");
            let detail = fit(&item.detail, detail_width);
            let size = item.size.map_or_else(String::new, human);
            let line = format!("{prefix}{name:<name_width$}  {detail}  {size:>8}");
            lines.push(render::clip(&line, width));
        } else if row >= view.len() {
            lines.push(String::new());
        }
    }
    lines.push("─".repeat(width));
    let ignored = selector.ignored().len();
    let footer = if searching {
        let prefix = format!(
            " {} matches  ·  {ignored} ignored  ·  /",
            visible_match_count(selector)
        );
        let query_width = width.saturating_sub(prefix.chars().count() + 1);
        let query: String = selector.filter.chars().take(query_width).collect();
        format!("{prefix}{query}_")
    } else {
        let mut footer = format!(
            " {} selected  ·  {ignored} ignored",
            selector.len().saturating_sub(ignored)
        );
        if selector.has_sizes() {
            footer.push_str(&format!(
                "  ·  download {}",
                human(selector.download_total())
            ));
        }
        if !selector.filter.is_empty() {
            footer.push_str(&format!(
                "  ·  Find: {}  ·  {} matches",
                selector.filter,
                visible_match_count(selector)
            ));
        } else {
            footer.push_str("  ·  / to search  ·  c clear ignores  ·  mouse wheel scroll");
        }
        footer
    };
    lines.push(paint(footer, "1", color));
    lines.truncate(height);
    lines
        .into_iter()
        .map(|line| render::clip(&line, width))
        .collect()
}

/// Runs the cursor-driven menu. The caller uses the text selector if raw mode fails.
pub fn run(title: &str, selector: &mut Selector, color: bool) -> io::Result<Option<Vec<String>>> {
    let _terminal = Terminal::enter()?;
    let (mut selected, mut offset, mut searching) = (0usize, 0usize, false);
    loop {
        if sys::interrupted() {
            return Ok(None);
        }
        let (width, height) = sys::term_size().unwrap_or((80, 24));
        let body_rows = height.saturating_sub(6).max(1);
        let view_len = selector.view().len();
        if view_len == 0 {
            selected = 0;
            offset = 0;
        } else {
            selected = selected.min(view_len - 1);
            if selected < offset {
                offset = selected;
            } else if selected >= offset + body_rows {
                offset = selected - body_rows + 1;
            }
        }
        let frame = render_menu(
            selector,
            MenuState {
                title,
                selected,
                offset,
                width,
                height,
                searching,
                color,
            },
        );
        let mut out = io::stdout().lock();
        out.write_all(b"\x1b[H\x1b[2J")?;
        for (i, line) in frame.iter().enumerate() {
            out.write_all(line.as_bytes())?;
            if i + 1 < frame.len() {
                out.write_all(b"\r\n")?;
            }
        }
        out.write_all(if searching {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        })?;
        out.flush()?;
        drop(out);

        let key = match read_key() {
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted && sys::interrupted() => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        match key {
            Key::Timeout | Key::Other => continue,
            Key::CtrlC => return Ok(None),
            Key::Up | Key::WheelUp => selected = selected.saturating_sub(1),
            Key::Down | Key::WheelDown => {
                if selected + 1 < view_len {
                    selected += 1;
                }
            }
            Key::PageUp => selected = selected.saturating_sub(body_rows),
            Key::PageDown => selected = (selected + body_rows).min(view_len.saturating_sub(1)),
            Key::Enter if searching => searching = false,
            Key::Enter => return Ok(Some(selector.ignored())),
            Key::Backspace if searching => {
                selector.filter.pop();
                selected = 0;
            }
            Key::Backspace => {}
            Key::Escape if searching => {
                selector.set_filter(String::new());
                searching = false;
                selected = 0;
            }
            Key::Escape => {}
            Key::Char(' ') => {
                if let Some((rank, _, _)) = selector.view().get(selected) {
                    selector.toggle_rank(*rank);
                }
                searching = false;
            }
            Key::Char('/') if !searching => searching = true,
            Key::Char('q' | 'Q') if !searching => return Ok(None),
            Key::Char('c' | 'C') if !searching => selector.clear_ignored(),
            Key::Char('s' | 'S') if !searching && selector.has_sizes() => {
                selector.sort = Sort::Size
            }
            Key::Char('n' | 'N') if !searching && selector.has_sizes() => {
                selector.sort = Sort::Name
            }
            Key::Char(c) if searching && !c.is_control() => {
                selector.filter.push(c);
                selected = 0;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::Item;

    fn selector() -> Selector {
        Selector::new(vec![
            Item {
                name: "alpha".into(),
                detail: "1 -> 2".into(),
                size: Some(100),
            },
            Item {
                name: "beta".into(),
                detail: "1 -> 2".into(),
                size: Some(50),
            },
            Item {
                name: "gamma".into(),
                detail: "1 -> 2".into(),
                size: Some(25),
            },
        ])
    }

    #[test]
    fn mouse_wheel_reports_are_decoded() {
        assert_eq!(decode_mouse(b"<64;20;4M"), Some(Key::WheelUp));
        assert_eq!(decode_mouse(b"<65;20;4M"), Some(Key::WheelDown));
        assert_eq!(decode_mouse(b"<0;20;4M"), None);
    }

    #[test]
    fn menu_is_bounded_to_terminal_height_and_highlights_search() {
        let mut s = selector();
        s.set_filter("be".into());
        let lines = render_menu(
            &s,
            MenuState {
                title: "Repository updates",
                selected: 0,
                offset: 0,
                width: 60,
                height: 8,
                searching: true,
                color: true,
            },
        );
        assert!(lines.len() <= 8);
        assert!(lines.join("\n").contains("\x1b[1;30;43mbe\x1b[0m"));
        assert!(lines.iter().any(|l| l.contains("1 matches")));
    }
}
