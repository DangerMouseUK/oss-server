//! One parsed config file (`config.c` `do_load` semantics) and ASSS's value
//! conversions.

use std::collections::HashMap;
use std::path::PathBuf;

use super::app::{ConfSource, Preprocessor};
use super::latin1;

/// ASSS `MAXSECTIONLEN`.
pub const MAX_SECTION_LEN: usize = 64;
/// ASSS `MAXKEYLEN` (the key buffer holds one byte less than this).
pub const MAX_KEY_LEN: usize = 64;

#[derive(Debug, Clone)]
struct Entry {
    key: String,
    value: String,
}

/// A config file with everything it includes, flattened into `Section:Key`
/// entries. Lookups are ASCII case-insensitive and later definitions replace
/// earlier ones, as in ASSS.
#[derive(Debug, Clone, Default)]
pub struct ConfigFile {
    path: PathBuf,
    sources: Vec<PathBuf>,
    table: HashMap<String, Entry>,
    warnings: Vec<String>,
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// `unescape_string`: copies until `stop` (or the end), dropping backslashes
/// before escaped characters, keeping at most `max - 1` bytes. Returns the
/// copied bytes and the unconsumed rest.
fn unescape(src: &[u8], max: usize, stop: Option<u8>) -> (Vec<u8>, &[u8]) {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() + 1 < max && i < src.len() && Some(src[i]) != stop {
        if src[i] == b'\\' {
            i += 1;
            if i < src.len() {
                out.push(src[i]);
                i += 1;
            }
        } else {
            out.push(src[i]);
            i += 1;
        }
    }
    (out, &src[i..])
}

impl ConfigFile {
    /// Loads `name` (resolved on the config search path for `arena`) and its
    /// includes. Returns `None` if the base file doesn't exist.
    pub fn load(src: &dyn ConfSource, arena: Option<&str>, name: &str) -> Option<Self> {
        let mut pp = Preprocessor::new(src, arena);
        if !pp.add_file(name) {
            return None;
        }
        let mut cf = ConfigFile {
            path: pp.opened().first().cloned().unwrap_or_default(),
            ..Default::default()
        };
        cf.parse(&mut pp);
        cf.sources = pp.opened().to_vec();
        let mut w = pp.take_errors();
        w.append(&mut cf.warnings);
        cf.warnings = w;
        Some(cf)
    }

    /// Parses config text without includes (convenience for tests and tools).
    pub fn from_text(text: &str) -> Self {
        let src = super::MemSource::new([("conf/text.conf", text)]);
        Self::load(&src, None, "text.conf").unwrap_or_default()
    }

    fn parse(&mut self, pp: &mut Preprocessor<'_>) {
        // current section prefix, e.g. b"Misc:"; empty before the first section
        let mut prefix: Vec<u8> = Vec::new();
        while let Some(line) = pp.next_line() {
            let start = line
                .iter()
                .position(|&b| !is_space(b))
                .unwrap_or(line.len());
            let end = line
                .iter()
                .rposition(|&b| !is_space(b))
                .map_or(start, |e| e + 1);
            let buf = &line[start..end.max(start)];

            if buf.first() == Some(&b'[') {
                let s = buf
                    .iter()
                    .position(|&b| b != b'[' && !is_space(b))
                    .unwrap_or(buf.len());
                let e = buf
                    .iter()
                    .rposition(|&b| b != b']' && !is_space(b))
                    .map_or(s, |e| e + 1);
                let mut section = buf[s..e.max(s)].to_vec();
                section.truncate(MAX_SECTION_LEN);
                prefix = section;
                prefix.push(b':');
                continue;
            }

            let (mut spot, rest) = unescape(buf, MAX_KEY_LEN, Some(b'='));
            if spot.is_empty() {
                continue;
            }
            if let Some(after_eq) = rest.strip_prefix(b"=") {
                while spot.last().is_some_and(|&b| is_space(b)) {
                    spot.pop();
                }
                let v = &after_eq[after_eq
                    .iter()
                    .position(|&b| !is_space(b))
                    .unwrap_or(after_eq.len())..];
                let (val, _) = unescape(v, super::app::MAX_LINE, None);
                let value = latin1(&val);
                if spot.contains(&b':') {
                    self.insert(latin1(&spot), value);
                } else if !prefix.is_empty() {
                    let mut k = prefix.clone();
                    k.extend_from_slice(&spot);
                    self.insert(latin1(&k), value);
                } else {
                    self.warnings
                        .push("ignoring value not in any section".to_owned());
                }
            } else {
                let mut k = prefix.clone();
                k.extend_from_slice(&spot);
                self.insert(latin1(&k), String::new());
            }
        }
    }

    fn insert(&mut self, key: String, value: String) {
        self.table
            .insert(key.to_ascii_lowercase(), Entry { key, value });
    }

    /// Resolved path of the base file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Every file read to build this one (the base file first, then includes).
    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    /// Problems found while loading (missing includes, stray values, ...).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Looks up a full `Section:Key` (or any raw key) string.
    pub fn get_raw(&self, full_key: &str) -> Option<&str> {
        self.table
            .get(&full_key.to_ascii_lowercase())
            .map(|e| e.value.as_str())
    }

    /// `GetStr(section, key)`.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let mut k = String::with_capacity(section.len() + key.len() + 1);
        k.push_str(section);
        k.push(':');
        k.push_str(key);
        k.make_ascii_lowercase();
        self.table.get(&k).map(|e| e.value.as_str())
    }

    /// `GetInt(section, key, default)`: see [`config_int`].
    pub fn get_int(&self, section: &str, key: &str, default: i32) -> i32 {
        self.get(section, key).map_or(default, config_int)
    }

    /// `GetInt` on a full `Section:Key` string.
    pub fn get_int_raw(&self, full_key: &str, default: i32) -> i32 {
        self.get_raw(full_key).map_or(default, config_int)
    }

    /// Sets a value in memory (`SetStr` without the write-back).
    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        self.insert(format!("{section}:{key}"), value.to_owned());
    }

    /// Iterates `(key, value)` pairs with keys in their original spelling.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.table
            .values()
            .map(|e| (e.key.as_str(), e.value.as_str()))
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// True when the file defined nothing.
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

/// C `strtol(s, &end, 0)` on a platform with 32-bit `long` (ASSS's target):
/// optional leading whitespace and sign, `0x` hex or leading-`0` octal,
/// saturating at the `i32` range. Returns `None` if no digits were consumed.
pub fn strtol32(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut radix = 10u32;
    if b.get(i) == Some(&b'0') {
        if matches!(b.get(i + 1), Some(b'x' | b'X'))
            && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            radix = 16;
            i += 2;
        } else {
            radix = 8;
        }
    }
    let mut mag: u64 = 0;
    let mut any = false;
    while let Some(d) = b.get(i).and_then(|&c| (c as char).to_digit(radix)) {
        any = true;
        mag = (mag * u64::from(radix) + u64::from(d)).min(1 << 32);
        i += 1;
    }
    if !any {
        return None;
    }
    let v = if neg { -(mag as i64) } else { mag as i64 };
    Some(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

/// ASSS `GetInt` conversion of a present value: the number if it parses,
/// otherwise 1 if the string starts with `y`/`Y` and 0 otherwise.
pub fn config_int(s: &str) -> i32 {
    strtol32(s).unwrap_or_else(|| i32::from(matches!(s.as_bytes().first(), Some(b'y' | b'Y'))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtol_matches_c() {
        assert_eq!(strtol32("42"), Some(42));
        assert_eq!(strtol32("  -17xyz"), Some(-17));
        assert_eq!(strtol32("0x1F"), Some(31));
        assert_eq!(strtol32("0X"), Some(0));
        assert_eq!(strtol32("0xg"), Some(0));
        assert_eq!(strtol32("010"), Some(8));
        assert_eq!(strtol32("09"), Some(0));
        assert_eq!(strtol32("+5"), Some(5));
        assert_eq!(strtol32("99999999999999999999"), Some(i32::MAX));
        assert_eq!(strtol32("-99999999999"), Some(i32::MIN));
        assert_eq!(strtol32("0xFFFFFFFF"), Some(i32::MAX));
        assert_eq!(strtol32("-2147483648"), Some(i32::MIN));
        assert_eq!(strtol32(""), None);
        assert_eq!(strtol32("-"), None);
        assert_eq!(strtol32("abc"), None);
        assert_eq!(config_int("yes"), 1);
        assert_eq!(config_int("Y"), 1);
        assert_eq!(config_int("no"), 0);
        assert_eq!(config_int("$SEE_TEAM"), 0);
    }

    #[test]
    fn sections_keys_and_overrides() {
        let cf = ConfigFile::from_text(
            "orphan = 1\n\
             [ Misc ]\n\
             BounceFactor = 22\n\
             bouncefactor=23   \n\
             Flag:CarryFlags = 1\n\
             NoValue\n\
             esc\\=aped = a\\\\b\\=c\n\
             [[Spawn]]\n\
             Team0-X = 512\n\
             Misc:Late = here\n\
             SpaceKey   =   spaced value  \n",
        );
        assert_eq!(cf.get("misc", "BOUNCEFACTOR"), Some("23"));
        assert_eq!(cf.get_int("Misc", "BounceFactor", 0), 23);
        assert_eq!(cf.get("Flag", "CarryFlags"), Some("1"));
        assert_eq!(cf.get("Misc", "NoValue"), Some(""));
        assert_eq!(cf.get("Misc", "esc=aped"), Some("a\\b=c"));
        assert_eq!(cf.get("Spawn", "Team0-X"), Some("512"));
        assert_eq!(cf.get("Misc", "Late"), Some("here"));
        assert_eq!(cf.get("Spawn", "SpaceKey"), Some("spaced value"));
        assert_eq!(cf.get_raw("orphan"), None);
        assert_eq!(cf.warnings().len(), 1);
        assert_eq!(cf.get_int("Misc", "Missing", -5), -5);
    }

    #[test]
    fn long_keys_truncate_like_asss() {
        let long = "k".repeat(70);
        let cf = ConfigFile::from_text(&format!("[S]\n{long} = 1\n"));
        // ASSS keeps 63 key bytes, then sees no '=' and stores an empty value
        assert_eq!(cf.get("S", &long[..63]), Some(""));
    }

    #[test]
    fn latin1_round_trips() {
        let src = crate::config::MemSource::new([(
            "conf/global.conf",
            &b"[Chat]\nGreeting = caf\xe9\n"[..],
        )]);
        let cf = ConfigFile::load(&src, None, "global.conf").unwrap_or_default();
        let v = cf.get("Chat", "Greeting").unwrap_or_default();
        assert_eq!(crate::config::to_latin1(v), b"caf\xe9");
    }
}
