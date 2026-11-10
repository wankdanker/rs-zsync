//! Small helpers: POSIX shell quoting, human byte sizes, comma lists.

/// Quote one word for a POSIX shell. Words made only of safe characters are left bare so logged
/// commands stay readable.
pub fn shell_quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_#".contains(c)) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Quote and join an argv for a POSIX shell.
pub fn shell_join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter().map(|w| shell_quote(w.as_ref())).collect::<Vec<_>>().join(" ")
}

/// Split a shell-ish word list on whitespace (used for `--zfs-path "sudo -n zfs"`). Quotes are not
/// interpreted: paths with spaces are not supported here.
pub fn split_words(text: &str) -> Vec<String> {
    text.split_whitespace().map(String::from).collect()
}

/// Parse `1024`, `10K`, `1.5M`, `2G`, `1T` (binary units, optional trailing `B`/`iB`) into bytes.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let lower = text.trim().to_ascii_lowercase();
    let t = lower.strip_suffix("ib").or_else(|| lower.strip_suffix('b')).unwrap_or(&lower);
    let (num, mult) = match t.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&t[..t.len() - 1], 1u64 << 10),
        Some('M') => (&t[..t.len() - 1], 1 << 20),
        Some('G') => (&t[..t.len() - 1], 1 << 30),
        Some('T') => (&t[..t.len() - 1], 1 << 40),
        Some('P') => (&t[..t.len() - 1], 1 << 50),
        _ => (t, 1),
    };
    let value: f64 = num.trim().parse().map_err(|_| format!("invalid size '{text}'"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid size '{text}'"));
    }
    Ok((value * mult as f64).round() as u64)
}

/// Format bytes for humans: `1.5 GiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Split a comma-separated list, ignoring commas inside `{...}` (glob brace sets) and dropping
/// empty items.
pub fn split_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in text.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth <= 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// `user@host` -> `host`.
pub fn bare_host(host: &str) -> &str {
    host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("pool/a@zsync-1"), "pool/a@zsync-1");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_join(&["zfs", "send", "-I", "p/a@1", "p/a@2"]), "zfs send -I p/a@1 p/a@2");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("10K").unwrap(), 10240);
        assert_eq!(parse_size("1.5M").unwrap(), 1572864);
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        assert_eq!(parse_size("3mb").unwrap(), 3 << 20);
        assert!(parse_size("fast").is_err());
        assert!(parse_size("-1").is_err());
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn lists() {
        assert_eq!(split_list("a/*, b/*"), vec!["a/*", "b/*"]);
        assert_eq!(split_list("p/{a,b},c"), vec!["p/{a,b}", "c"]);
        assert_eq!(split_list(""), Vec::<String>::new());
        assert_eq!(split_list(",x,,"), vec!["x"]);
    }

    #[test]
    fn hosts() {
        assert_eq!(bare_host("root@backup"), "backup");
        assert_eq!(bare_host("backup"), "backup");
    }
}
