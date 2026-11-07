//! The ASSS config preprocessor (`app.c`), reproduced line for line.
//!
//! Supported, exactly as ASSS 1.4.4 does it:
//! - `#include file` (also `"file"` / `<file>`), resolved on the config search
//!   path for the arena being loaded;
//! - `#define NAME [value]`, `#undef NAME`, `#ifdef NAME`, `#ifndef NAME`,
//!   `#else`, `#endif` (names are case-insensitive; `$NAME`/`${NAME}` values are
//!   *not* expanded, matching ASSS 1.4.4 whose expansion is documented but
//!   unimplemented);
//! - a trailing `\` joins the next physical line (even across files);
//! - lines whose first non-blank character is `;` or `/` are comments.
//!
//! Directives must start in column 0. Lines are byte strings; the config layer
//! decodes them as Latin-1 so that every byte survives unchanged.
//!
//! Deliberate divergences (only in cases where ASSS has undefined behaviour or
//! loops forever): include nesting is capped at [`MAX_INCLUDE_DEPTH`] and a
//! directive too short for its argument is treated as having an empty one.

use std::collections::HashMap;
use std::path::PathBuf;

/// ASSS `CFG_MAX_LINE`: physical lines are read in chunks of at most this many
/// bytes minus one, like `fgets`.
pub const MAX_LINE: usize = 4096;
/// Maximum nesting of `#include` (ASSS: `MAX_RECURSION_DEPTH`).
pub const MAX_INCLUDE_DEPTH: usize = 50;

/// Somewhere config files can be found: a zone directory on disk, or an
/// in-memory set of files in tests.
pub trait ConfSource {
    /// Finds the first file on an ASSS search path (`%n`, `%b`, `%m` macros
    /// in `repls`), returning the path it is known by.
    fn find(&self, search_path: &str, repls: &[(char, &str)]) -> Option<PathBuf>;

    /// Reads a file returned by [`ConfSource::find`] (or any zone-relative
    /// path for in-memory sources).
    fn read(&self, path: &std::path::Path) -> Option<Vec<u8>>;

    /// When the file last changed, if the source can tell (for reloading).
    fn modified(&self, _path: &std::path::Path) -> Option<std::time::SystemTime> {
        None
    }

    /// Locates `name` on the config search path for `arena` (or the global
    /// path when `arena` is `None`) and returns its resolved path and bytes.
    fn open(&self, arena: Option<&str>, name: &str) -> Option<(PathBuf, Vec<u8>)> {
        let mut repls = vec![('n', name)];
        if let Some(a) = arena {
            repls.push(('b', a));
        }
        let path = self.find(super::search::CONFIG_SEARCH_PATH, &repls)?;
        let data = self.read(&path)?;
        Some((path, data))
    }

    /// Appends text to a file previously returned by [`ConfSource::open`] (used
    /// to write changed settings back, as ASSS does). Sources that can't be
    /// written return `Ok(false)`.
    fn append(&self, _path: &std::path::Path, _text: &[u8]) -> std::io::Result<bool> {
        Ok(false)
    }
}

impl<T: ConfSource + ?Sized> ConfSource for Box<T> {
    fn find(&self, search_path: &str, repls: &[(char, &str)]) -> Option<PathBuf> {
        (**self).find(search_path, repls)
    }

    fn read(&self, path: &std::path::Path) -> Option<Vec<u8>> {
        (**self).read(path)
    }

    fn modified(&self, path: &std::path::Path) -> Option<std::time::SystemTime> {
        (**self).modified(path)
    }

    fn open(&self, arena: Option<&str>, name: &str) -> Option<(PathBuf, Vec<u8>)> {
        (**self).open(arena, name)
    }

    fn append(&self, path: &std::path::Path, text: &[u8]) -> std::io::Result<bool> {
        (**self).append(path, text)
    }
}

struct OpenFile {
    path: PathBuf,
    data: Vec<u8>,
    pos: usize,
    lineno: u32,
}

impl OpenFile {
    /// `fgets(buf, n)`: up to `n - 1` bytes, stopping after a newline.
    fn fgets(&mut self, n: usize) -> Option<&[u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let max = n.saturating_sub(1);
        let rest = &self.data[self.pos..];
        let mut len = rest.len().min(max);
        if let Some(nl) = rest[..len].iter().position(|&b| b == b'\n') {
            len = nl + 1;
        }
        let start = self.pos;
        self.pos += len;
        Some(&self.data[start..start + len])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Where {
    InIf,
    InElse,
}

struct IfBlock {
    cond: bool,
    place: Where,
}

/// Streams logical lines out of a config file and everything it includes.
pub struct Preprocessor<'a> {
    src: &'a dyn ConfSource,
    arena: Option<String>,
    /// File stack; the last element is the one being read.
    files: Vec<OpenFile>,
    ifs: Vec<IfBlock>,
    processing: bool,
    defs: HashMap<String, String>,
    errors: Vec<String>,
    /// Every file that was opened, in order (for change detection).
    opened: Vec<PathBuf>,
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn trim_end_by(s: &[u8], f: impl Fn(u8) -> bool) -> &[u8] {
    let mut end = s.len();
    while end > 0 && f(s[end - 1]) {
        end -= 1;
    }
    &s[..end]
}

fn trim_start_by(s: &[u8], f: impl Fn(u8) -> bool) -> &[u8] {
    let mut start = 0;
    while start < s.len() && f(s[start]) {
        start += 1;
    }
    &s[start..]
}

fn def_key(name: &[u8]) -> String {
    super::latin1(name).to_ascii_lowercase()
}

impl<'a> Preprocessor<'a> {
    /// Creates a preprocessor resolving includes for `arena`.
    pub fn new(src: &'a dyn ConfSource, arena: Option<&str>) -> Self {
        Self {
            src,
            arena: arena.map(str::to_owned),
            files: Vec::new(),
            ifs: Vec::new(),
            processing: true,
            defs: HashMap::new(),
            errors: Vec::new(),
            opened: Vec::new(),
        }
    }

    /// Defines a macro before reading (`APPAddDef`).
    pub fn define(&mut self, name: &str, value: &str) {
        self.defs
            .insert(name.to_ascii_lowercase(), value.to_owned());
    }

    /// Warnings collected so far (missing includes, stray `#else`, ...).
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Takes the collected warnings.
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    /// Paths of every file opened while reading.
    pub fn opened(&self) -> &[PathBuf] {
        &self.opened
    }

    fn get_file(&mut self, name: &str) -> Option<OpenFile> {
        match self.src.open(self.arena.as_deref(), name) {
            Some((path, data)) => {
                self.opened.push(path.clone());
                Some(OpenFile {
                    path,
                    data,
                    pos: 0,
                    lineno: 0,
                })
            }
            None => {
                self.errors.push(format!(
                    "Can't find file for arena '{}', name '{}'",
                    self.arena.as_deref().unwrap_or("(null)"),
                    name
                ));
                None
            }
        }
    }

    /// Queues a file to be read after the ones already queued (`APPAddFile`).
    /// Returns false when the file could not be found.
    pub fn add_file(&mut self, name: &str) -> bool {
        if self.files.len() >= MAX_INCLUDE_DEPTH {
            self.errors.push(format!(
                "Maximum #include recursion depth reached while adding '{name}'"
            ));
            return false;
        }
        match self.get_file(name) {
            Some(f) => {
                self.files.insert(0, f);
                true
            }
            None => false,
        }
    }

    fn location(&self) -> String {
        match self.files.last() {
            Some(f) => format!("{}:{}", f.path.display(), f.lineno),
            None => "<eof>".to_owned(),
        }
    }

    fn update_processing(&mut self) {
        self.processing = self.ifs.iter().all(|i| {
            // ASSS: processing stops when cond == where (true/in_else or false/in_if)
            !((i.cond && i.place == Where::InElse) || (!i.cond && i.place == Where::InIf))
        });
    }

    fn handle_directive(&mut self, line: &[u8]) {
        let buf = &line[1..];
        let rest = |n: usize| buf.get(n..).unwrap_or(&[]);
        if buf.starts_with(b"ifdef") || buf.starts_with(b"ifndef") {
            let name = trim_end_by(buf, |b| is_space(b) || b == b'}' || b == b')');
            let name = name.get(6..).unwrap_or(&[]);
            let name = trim_start_by(name, |b| is_space(b) || b == b'{' || b == b'(');
            let defined = self.defs.contains_key(&def_key(name));
            let cond = if buf[2] == b'd' { defined } else { !defined };
            self.ifs.push(IfBlock {
                cond,
                place: Where::InIf,
            });
            self.update_processing();
        } else if buf.starts_with(b"else") {
            let loc = self.location();
            match self.ifs.last_mut() {
                Some(i) if i.place == Where::InIf => i.place = Where::InElse,
                Some(_) => self
                    .errors
                    .push(format!("Multiple #else directives ({loc})")),
                None => self
                    .errors
                    .push(format!("Unexpected #else directive ({loc})")),
            }
            self.update_processing();
        } else if buf.starts_with(b"endif") {
            if self.ifs.pop().is_none() {
                let loc = self.location();
                self.errors.push(format!("No #if blocks to end ({loc})"));
            }
            self.update_processing();
        } else if !self.processing {
        } else if buf.starts_with(b"define") {
            let t = trim_start_by(rest(7), is_space);
            let key_len = t.iter().position(|&b| is_space(b)).unwrap_or(t.len());
            let key = def_key(&t[..key_len]);
            let value = if key_len < t.len() {
                super::latin1(trim_start_by(&t[key_len + 1..], is_space))
            } else {
                "1".to_owned()
            };
            self.defs.insert(key, value);
        } else if buf.starts_with(b"undef") {
            let t = trim_start_by(trim_end_by(rest(5), is_space), is_space);
            self.defs.remove(&def_key(t));
        } else if buf.starts_with(b"include") {
            let t = trim_end_by(rest(7), |b| is_space(b) || b == b'"' || b == b'>');
            let t = trim_start_by(t, |b| is_space(b) || b == b'"' || b == b'<');
            let name = super::latin1(t);
            if self.files.len() >= MAX_INCLUDE_DEPTH {
                let loc = self.location();
                self.errors.push(format!(
                    "Maximum #include recursion depth reached while including '{name}' ({loc})"
                ));
            } else if let Some(f) = self.get_file(&name) {
                self.files.push(f);
            }
        }
    }

    /// Returns the next logical line (without its line ending), or `None` at
    /// the end of all input. Directives, comments and blank lines are consumed
    /// here; the caller only sees content lines.
    pub fn next_line(&mut self) -> Option<Vec<u8>> {
        loop {
            let mut buf: Vec<u8> = Vec::new();
            // gather one physical line plus any `\` continuations
            loop {
                let chunk = loop {
                    let top = self.files.last_mut()?;
                    let cap = MAX_LINE - buf.len();
                    match top.fgets(cap).map(<[u8]>::to_vec) {
                        Some(c) => {
                            top.lineno += 1;
                            break c;
                        }
                        None => {
                            self.files.pop();
                        }
                    }
                };
                // C strings end at the first NUL
                let chunk = match chunk.iter().position(|&b| b == 0) {
                    Some(n) => &chunk[..n],
                    None => &chunk[..],
                };
                // RemoveCRLF: cut at the first LF, then at the first CR
                let chunk = match chunk.iter().position(|&b| b == b'\n') {
                    Some(n) => &chunk[..n],
                    None => chunk,
                };
                let chunk = match chunk.iter().position(|&b| b == b'\r') {
                    Some(n) => &chunk[..n],
                    None => chunk,
                };
                buf.extend_from_slice(chunk);
                if buf.last() == Some(&b'\\') {
                    buf.pop();
                } else {
                    break;
                }
            }

            if buf.first() == Some(&b'#') {
                self.handle_directive(&buf);
                continue;
            }
            match buf.iter().find(|&&b| !is_space(b)) {
                None | Some(b';') | Some(b'/') => continue,
                Some(_) => {}
            }
            if self.processing {
                return Some(buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemSource;

    fn lines(src: &MemSource, arena: Option<&str>, name: &str) -> (Vec<String>, Vec<String>) {
        let mut pp = Preprocessor::new(src, arena);
        pp.add_file(name);
        let mut out = Vec::new();
        while let Some(l) = pp.next_line() {
            out.push(crate::config::latin1(&l));
        }
        (out, pp.take_errors())
    }

    #[test]
    fn comments_blanks_and_continuations() {
        let src = MemSource::new([(
            "conf/global.conf",
            "; comment\n  / also comment\n\n[A]\nkey = a \\\n\tb \\\n c\r\nlast\n",
        )]);
        let (l, e) = lines(&src, None, "global.conf");
        assert!(e.is_empty());
        assert_eq!(l, vec!["[A]", "key = a \tb  c", "last"]);
    }

    #[test]
    fn indented_hash_is_not_a_directive() {
        let src = MemSource::new([("conf/global.conf", "  #include x\n")]);
        let (l, e) = lines(&src, None, "global.conf");
        assert_eq!(l, vec!["  #include x"]);
        assert!(e.is_empty());
    }

    #[test]
    fn includes_resolve_per_arena_and_nest() {
        let src = MemSource::new([
            ("arenas/turf/arena.conf", "a\n#include \"shared.conf\"\nb\n"),
            ("arenas/turf/shared.conf", "turf-shared\n#include <deep>\n"),
            ("conf/shared.conf", "global-shared\n"),
            ("conf/deep", "deep\n"),
        ]);
        let (l, e) = lines(&src, Some("turf"), "arena.conf");
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(l, vec!["a", "turf-shared", "deep", "b"]);
        let (l, _) = lines(&src, None, "shared.conf");
        assert_eq!(l, vec!["global-shared"]);
    }

    #[test]
    fn missing_include_is_reported() {
        let src = MemSource::new([("conf/global.conf", "#include nope\nx\n")]);
        let (l, e) = lines(&src, None, "global.conf");
        assert_eq!(l, vec!["x"]);
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn self_include_terminates() {
        let src = MemSource::new([("conf/global.conf", "x\n#include global.conf\n")]);
        let (l, e) = lines(&src, None, "global.conf");
        assert_eq!(l.len(), MAX_INCLUDE_DEPTH);
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn conditionals() {
        let text = "#define FOO\n#define Bar some value\n\
            #ifdef foo\nfoo-yes\n#else\nfoo-no\n#endif\n\
            #ifndef (BAR)\nbar-no\n#else\nbar-yes\n#endif\n\
            #ifdef ${BAR}\ndollar-is-literal\n#endif\n\
            #ifdef nothing\n#ifdef foo\nhidden\n#endif\n#define LATE\n#else\nshown\n#endif\n\
            #ifdef LATE\nlate\n#endif\n\
            #undef FOO\n#ifdef FOO\nundef-failed\n#endif\n\
            #else\n#endif\n";
        let src = MemSource::new([("conf/global.conf", text)]);
        let (l, e) = lines(&src, None, "global.conf");
        assert_eq!(l, vec!["foo-yes", "bar-yes", "shown"]);
        assert_eq!(e.len(), 2, "{e:?}");
    }

    #[test]
    fn hostile_input_does_not_panic() {
        let text = "#\n#i\n#ifdef\n#ifndef\n#define\n#undef\n#include\n#else\n#else\n\\";
        let src = MemSource::new([("conf/global.conf", text)]);
        let _ = lines(&src, None, "global.conf");
        let long = "x".repeat(10_000);
        let src = MemSource::new([("conf/global.conf", long.as_str())]);
        let (l, _) = lines(&src, None, "global.conf");
        assert_eq!(l.iter().map(String::len).sum::<usize>(), 10_000);
    }
}
