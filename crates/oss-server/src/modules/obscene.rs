//! `obscene`: the obscenity filter (ASSS `obscene.c`).
//!
//! `Chat:Obscene` lists words to filter, separated by spaces; a word starting
//! with `?` is rot-13 encoded. Players with the filter on (from their arena
//! request, `Chat:ForceFilter`, or `?obscene`) get matching words replaced by
//! symbols; with `Chat:FilterMode = 0` they don't get such lines at all.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::chat::message;

const REPLACE: &[u8] = b"%@$&%*!#@&%!#&*$#?@!*%@&!%#&%!?$*#!*$&@#&%$!*%@#&%!@&#$!*@&$%*@?";

/// The filter (ASSS `Iobscene`).
pub trait Obscene: Send + Sync {
    /// Replaces obscene words in `line`; returns whether any were found.
    fn filter(&self, zone: &Zone, line: &mut String) -> bool;

    /// The module-wide replacement counter (ASSS `replace_count`). It is
    /// never reset while the module is loaded, not even when the word list
    /// is re-read, so the same word is masked differently every time --
    /// across lines, arenas and players.
    fn replace_count(&self) -> usize;

    /// Sets that counter. Only for replaying a recorded transcript from a
    /// known offset, which is the one thing ASSS's own harness reaches in
    /// and changes.
    fn set_replace_count(&self, n: usize);
}

#[derive(Default)]
struct Words(Vec<String>);

/// Parses `Chat:Obscene` (lowercased; `?` marks rot-13).
pub fn parse_words(s: &str) -> Vec<String> {
    s.split([' ', '\t'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let w = w.to_ascii_lowercase();
            match w.strip_prefix('?') {
                Some(enc) => enc
                    .chars()
                    .map(|c| match c {
                        'a'..='m' => ((c as u8) + 13) as char,
                        'n'..='z' => ((c as u8) - 13) as char,
                        c => c,
                    })
                    .collect(),
                None => w,
            }
        })
        .collect()
}

/// The filter implementation.
pub struct ObsceneImpl {
    count: AtomicUsize,
}

impl Obscene for ObsceneImpl {
    fn filter(&self, zone: &Zone, line: &mut String) -> bool {
        let Some(words) = zone.res.get::<Words>() else {
            return false;
        };
        // work on Latin-1 bytes, like ASSS
        let mut bytes = crate::config::to_latin1(line);
        let mut hit = false;
        for w in &words.0 {
            let wb = w.as_bytes();
            if wb.is_empty() || wb.len() > bytes.len() {
                continue;
            }
            let mut i = 0;
            while i + wb.len() <= bytes.len() {
                if bytes[i..i + wb.len()].eq_ignore_ascii_case(wb) {
                    hit = true;
                    let pos = (5 * self.count.fetch_add(1, Ordering::Relaxed)) % REPLACE.len();
                    for (k, b) in bytes[i..i + wb.len()].iter_mut().enumerate() {
                        *b = REPLACE[(pos + k) % REPLACE.len()];
                    }
                    i += wb.len();
                } else {
                    i += 1;
                }
            }
        }
        if hit {
            *line = crate::config::latin1(&bytes);
        }
        hit
    }

    fn replace_count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    fn set_replace_count(&self, n: usize) {
        self.count.store(n, Ordering::Relaxed);
    }
}

/// Re-reads `Chat:Obscene`. The replacement counter is deliberately left
/// alone, as in ASSS.
pub fn reload_words(zone: &mut Zone) {
    load(zone);
}

fn load(zone: &mut Zone) {
    let words = zone
        .cfg
        .global()
        .get("Chat", "Obscene")
        .map(parse_words)
        .unwrap_or_default();
    if !words.is_empty() {
        tracing::info!(target: "obscene", count = words.len(), "loaded obscene words");
    }
    zone.res.insert(Words(words));
}

fn c_obscene(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(p) = zone.players.get_mut(inv.sender) else {
        return;
    };
    p.obscenity_filter = !p.obscenity_filter;
    let on = p.obscenity_filter;
    message(
        zone,
        inv.sender,
        if on {
            "Obscene filter ON"
        } else {
            "Obscene filter OFF"
        },
    );
}

/// The `obscene` module.
pub struct ObsceneModule;

impl Module for ObsceneModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        load(zone);
        zone.reg.provide::<dyn Obscene>(
            me,
            Scope::Global,
            Arc::new(ObsceneImpl {
                count: AtomicUsize::new(0),
            }),
        );
        zone.add_command(
            me,
            "obscene",
            None,
            Some("Module: obscene\nTargets: none\nArgs: none\nToggles the obscene word filter.\n"),
            c_obscene,
        );
        // reload with the global config
        zone.set_timer(me, 1500, 1500, |z| {
            load(z);
            true
        });
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<Words>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rot13_words() {
        assert_eq!(
            parse_words("Darn ?qnea\theck"),
            vec!["darn", "darn", "heck"]
        );
    }
}
