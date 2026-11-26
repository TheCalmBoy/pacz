//! Human-readable sizes, parsing of aria2's size strings, ANSI stripping.

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

pub fn human(bytes: u64) -> String {
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    // 1023.97KiB would otherwise print as "1024KiB".
    if v >= 1023.95 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || (v - v.round()).abs() < 0.05 {
        format!("{:.0}{}", v, UNITS[u])
    } else {
        format!("{:.1}{}", v, UNITS[u])
    }
}

/// Parses "512", "512B", "1.5KiB", "10MiB", "2GiB", "1.5KiB/s" (and bare K/M/G/T) into bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s.strip_suffix("/s").unwrap_or(s).trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    if num.is_empty() {
        return None;
    }
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "B" | "" => 1.0,
        "K" | "KiB" => 1024.0,
        "M" | "MiB" => 1024.0 * 1024.0,
        "G" | "GiB" => 1024.0_f64.powi(3),
        "T" | "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((n * mult).round() as u64)
}

/// Removes ANSI CSI sequences (colors, cursor movement) from a line.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if it.peek() == Some(&'[') {
            it.next();
            // Parameters and intermediates, then one final byte in 0x40..=0x7e.
            for d in it.by_ref() {
                if ('\x40'..='\x7e').contains(&d) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(1023), "1023B");
        assert_eq!(human(1536), "1.5KiB");
        assert_eq!(human(10 << 20), "10MiB");
        assert_eq!(human(3 << 30), "3GiB");
        assert_eq!(human((1 << 20) - 10), "1MiB");
    }
    #[test]
    fn parses() {
        assert_eq!(parse_size("1.5KiB"), Some(1536));
        assert_eq!(parse_size("10MiB"), Some(10 << 20));
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("1.5KiB/s"), Some(1536));
        assert_eq!(parse_size("junk"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("5XB"), None);
    }
    #[test]
    fn strips_colors() {
        assert_eq!(strip_ansi("\x1b[1;32mfoo\x1b[0m 1 -> 2"), "foo 1 -> 2");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
