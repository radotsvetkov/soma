//! Shared utilities: time/civil-date math, ids, globs, atomic file IO.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub type R<T> = Result<T, String>;

pub fn ctx<T, E: std::fmt::Display>(r: Result<T, E>, what: &str) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

// ---------- time ----------

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Days-from-civil / civil-from-days (Howard Hinnant's algorithms).
/// soma keeps all timestamps in UTC; cron evaluation is UTC too (documented).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 0 = Sunday … 6 = Saturday (cron convention).
pub fn weekday_from_days(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

pub struct UtcParts {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub weekday: u32, // 0=Sun
}

pub fn utc_parts(ms: i64) -> UtcParts {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    UtcParts {
        year,
        month,
        day,
        hour: (sod / 3600) as u32,
        minute: ((sod % 3600) / 60) as u32,
        second: (sod % 60) as u32,
        weekday: weekday_from_days(days),
    }
}

/// RFC3339 / ISO-8601 UTC timestamp with millisecond precision.
pub fn iso8601(ms: i64) -> String {
    let p = utc_parts(ms);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        p.year,
        p.month,
        p.day,
        p.hour,
        p.minute,
        p.second,
        ms.rem_euclid(1000)
    )
}

// ---------- ids ----------

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn random_u64() -> u64 {
    // 8 bytes from /dev/urandom; fall back to a time/pid mix.
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut buf = [0u8; 8];
        if f.read_exact(&mut buf).is_ok() {
            return u64::from_le_bytes(buf);
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(std::process::id() as u64)
}

/// Sortable unique id: `<prefix>_<millis-hex><counter><random-hex>`.
pub fn new_id(prefix: &str) -> String {
    let c = COUNTER.fetch_add(1, Ordering::Relaxed) % 4096;
    format!(
        "{}_{:011x}{:03x}{:08x}",
        prefix,
        now_ms(),
        c,
        (random_u64() & 0xffff_ffff) as u32
    )
}

// ---------- strings ----------

/// Lowercase alphanumeric tokens, length ≥ 2. Shared by the selector (R6)
/// and knowledge search (R8) so scoring is consistent.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            if cur.len() >= 2 {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() >= 2 {
        out.push(cur);
    }
    out
}

/// Glob match supporting `*` (any sequence) and `?` (single char).
/// Used by the policy engine for command/path patterns (R3).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

// ---------- fs ----------

pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn ensure_dir(path: &Path) -> R<()> {
    ctx(fs::create_dir_all(path), &format!("create dir {}", path.display()))
}

/// Write via temp file + rename so readers never observe a torn file.
pub fn atomic_write(path: &Path, content: &[u8]) -> R<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    ctx(fs::write(&tmp, content), &format!("write {}", tmp.display()))?;
    ctx(fs::rename(&tmp, path), &format!("rename into {}", path.display()))
}

pub fn read_to_string(path: &Path) -> R<String> {
    ctx(fs::read_to_string(path), &format!("read {}", path.display()))
}

/// Append one line (adds trailing newline) to a file, creating it if needed.
pub fn append_line(path: &Path, line: &str) -> R<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let mut f = ctx(
        fs::OpenOptions::new().create(true).append(true).open(path),
        &format!("open {}", path.display()),
    )?;
    ctx(f.write_all(line.as_bytes()), "append line")?;
    ctx(f.write_all(b"\n"), "append newline")
}

/// Stream lines of a file (memory-bounded; R18) through a callback.
/// Missing file is treated as empty, not an error.
pub fn for_each_line(path: &Path, mut f: impl FnMut(&str) -> R<()>) -> R<()> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("open {}: {e}", path.display())),
    };
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = ctx(line, "read line")?;
        if !line.trim().is_empty() {
            f(&line)?;
        }
    }
    Ok(())
}

/// Last `n` non-empty lines of a file (single bounded pass).
pub fn tail_lines(path: &Path, n: usize) -> R<Vec<String>> {
    let mut buf: std::collections::VecDeque<String> = std::collections::VecDeque::with_capacity(n);
    for_each_line(path, |line| {
        if buf.len() == n {
            buf.pop_front();
        }
        buf.push_back(line.to_string());
        Ok(())
    })?;
    Ok(buf.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip_and_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(weekday_from_days(0), 4); // 1970-01-01 was a Thursday
        assert_eq!(days_from_civil(2026, 6, 10), 20614);
        assert_eq!(civil_from_days(20614), (2026, 6, 10));
        assert_eq!(weekday_from_days(days_from_civil(2026, 6, 10)), 3); // Wednesday
        for days in [-1000i64, -1, 0, 1, 59, 60, 365, 36524, 20614] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        // leap year handling
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn iso_format() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        // 20614 days (2026-06-10) * 86400 + 12h40m → ms
        assert_eq!(iso8601(1_781_095_200_123), "2026-06-10T12:40:00.123Z");
    }

    #[test]
    fn ids_unique_and_sortable() {
        let a = new_id("ev");
        let b = new_id("ev");
        assert_ne!(a, b);
        assert!(a.starts_with("ev_"));
        assert_eq!(a.len(), b.len()); // fixed width → lexicographically sortable
    }

    #[test]
    fn glob() {
        assert!(glob_match("cargo *", "cargo build --release"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("rm -rf *", "rm -rf /"));
        assert!(!glob_match("git push*", "git pull"));
        assert!(glob_match("?at", "cat"));
        assert!(!glob_match("?at", "flat"));
        assert!(glob_match("*secret*", "my_secret_key"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn tokenizer() {
        assert_eq!(
            tokenize("Run Cargo-BUILD, then test! a"),
            vec!["run", "cargo", "build", "then", "test"]
        );
    }

    #[test]
    fn tail_and_append() {
        let dir = std::env::temp_dir().join(format!("soma-test-{}", new_id("t")));
        let p = dir.join("x.log");
        for i in 0..10 {
            append_line(&p, &format!("line{i}")).unwrap();
        }
        assert_eq!(tail_lines(&p, 3).unwrap(), vec!["line7", "line8", "line9"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("soma-test-{}", new_id("t")));
        let p = dir.join("a/b/c.json");
        atomic_write(&p, b"{}").unwrap();
        assert_eq!(read_to_string(&p).unwrap(), "{}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
