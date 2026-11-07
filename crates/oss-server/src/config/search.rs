//! File lookup on ASSS-style search paths (`pathutil.c` semantics).
//!
//! A search path is a `:`-separated list of templates such as
//! `arenas/%b/%n:conf/%n:%n`. `%n` expands to the requested file name and `%b`
//! to the arena base name; `%%` is a literal `%`. A template that uses a macro
//! with no value (for example `%b` when looking up a global file) is skipped,
//! exactly like ASSS's `find_file_on_path`.

use std::path::{Path, PathBuf};

/// Search path ASSS uses for `.conf` files (`CFG_CONFIG_SEARCH_PATH`).
pub const CONFIG_SEARCH_PATH: &str = "arenas/%b/%n:conf/%n:%n:arenas/(default)/%n";
/// Search path ASSS uses for `.lvl` maps (`CFG_LVL_SEARCH_PATH`); `%m` is the map name.
pub const LVL_SEARCH_PATH: &str =
    "arenas/%b/%m:maps/%m:%m:arenas/%b/%b.lvl:maps/%b.lvl:arenas/(default)/%m";
/// Search path ASSS uses for `.lvz` files (`CFG_LVZ_SEARCH_PATH`); `%m` is the file name.
pub const LVZ_SEARCH_PATH: &str = "arenas/%b/%m:maps/%m:%m:arenas/(default)/%m";

/// Expands one template. Returns `None` when a macro has no replacement.
pub fn expand_template(template: &str, repls: &[(char, &str)]) -> Option<String> {
    let mut out = String::with_capacity(template.len() + 16);
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let m = chars.next()?;
        if m == '%' {
            out.push('%');
            continue;
        }
        let (_, with) = repls.iter().find(|(r, _)| *r == m)?;
        out.push_str(with);
    }
    Some(out)
}

/// Returns the first expansion of `search_path` for which `exists` holds.
pub fn find_on_path(
    search_path: &str,
    repls: &[(char, &str)],
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    search_path
        .split(':')
        .filter_map(|t| expand_template(t, repls))
        .find(|rel| exists(rel))
}

/// Finds the first existing regular file on `search_path`, relative to `root`.
pub fn find_file_on_path(
    root: &Path,
    search_path: &str,
    repls: &[(char, &str)],
) -> Option<PathBuf> {
    find_on_path(search_path, repls, |rel| root.join(rel).is_file()).map(|rel| root.join(rel))
}

/// ASSS `is_valid_path`: rejects absolute paths, `..`, `//`, `:` and
/// non-printable characters. Used before touching files named by clients or
/// by configuration that flows from clients (e.g. arena names).
pub fn is_valid_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.first() == Some(&b'/') || bytes.last() == Some(&b'/') {
        return false;
    }
    let mut last = 0u8;
    for &ch in bytes {
        if !(b' '..=b'~').contains(&ch) || ch == b':' {
            return false;
        }
        if matches!(ch, b'.' | b'/' | b'\\') && last == ch {
            return false;
        }
        last = ch;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_expand_or_skip() {
        let r = [('n', "arena.conf"), ('b', "turf")];
        assert_eq!(
            expand_template("arenas/%b/%n", &r).as_deref(),
            Some("arenas/turf/arena.conf")
        );
        assert_eq!(expand_template("100%%", &r).as_deref(), Some("100%"));
        assert_eq!(expand_template("arenas/%b/%n", &r[..1]), None);
        assert_eq!(expand_template("trailing%", &r), None);
    }

    #[test]
    fn path_validation() {
        assert!(is_valid_path("maps/foo.lvl"));
        assert!(!is_valid_path("/etc/passwd"));
        assert!(!is_valid_path("../x"));
        assert!(!is_valid_path("a//b"));
        assert!(!is_valid_path("c:x"));
        assert!(!is_valid_path("dir/"));
    }
}
