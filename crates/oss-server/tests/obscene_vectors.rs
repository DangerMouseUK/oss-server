//! `obscene` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/obscene.txt`, from `core/obscene.c` compiled
//! unmodified).
//!
//! The rule a port is most likely to miss is that `replace_count` is one
//! module-wide counter that is never reset while the module is loaded — not
//! even when the word list is re-read. So the same word is masked
//! differently every time, and an implementation that derives the mask from
//! the line matches the first record and then diverges.

use oss_server::config::MemSource;
use oss_server::modules::obscene::{self, Obscene};
use oss_server::runtime::ServerBuilder;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/obscene.txt");

fn zone_with(setting: &str) -> Zone {
    let src = MemSource::new([
        ("conf/global.conf", format!("[Chat]\nObscene = {setting}\n")),
        (
            "conf/passwd.conf",
            "[General]\nAllowUnknown = yes\n".to_owned(),
        ),
        ("conf/groupdef.conf", "[default]\n".to_owned()),
        ("conf/staff.conf", String::new()),
        ("arenas/(default)/arena.conf", String::new()),
    ]);
    ServerBuilder::with_source(src).build_zone().expect("zone")
}

/// Changes `Chat:Obscene` and re-reads the word list, the way the config
/// reload timer does. The replacement counter is left alone, which is what
/// the transcript is checking.
fn set_obscene(zone: &mut Zone, setting: &str) {
    let file = zone.cfg.global().clone();
    // `set` refreshes the manager's own global handle when it is that file
    let _ = zone.cfg.set(&file, "Chat", "Obscene", setting, None, false);
    obscene::reload_words(zone);
}

/// `[` ... `]`, allowing the content to hold brackets of its own.
fn bracketed(s: &str) -> Option<(String, &str)> {
    let open = s.find('[')?;
    let close = s.rfind(']')?;
    if close < open {
        return None;
    }
    Some((s[open + 1..close].to_owned(), &s[close + 1..]))
}

/// The first `[...]` only, for records with several in a row.
fn first_bracketed(s: &str) -> Option<(String, &str)> {
    let open = s.find('[')?;
    let close = s[open..].find(']')? + open;
    Some((s[open + 1..close].to_owned(), &s[close + 1..]))
}

#[test]
fn obscene_matches_asss() {
    // one zone for the whole file: the counter has to survive the word-list
    // reloads between scenarios, which is half of what is being tested
    let mut zone = zone_with("");
    let filter = zone.iface::<dyn Obscene>().expect("obscene");
    let mut label = String::new();
    let mut checked = 0usize;
    let mut skip_output = false;
    let mut prev_was_count = false;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(c) = line.strip_prefix("# ") {
            if let Some((setting, _)) =
                c.rsplit_once(": Chat:Obscene = ").and_then(|(text, rest)| {
                    label = text.to_owned();
                    bracketed(&format!("x{rest}")).map(|(s, r)| (s, r.to_owned()))
                })
            {
                set_obscene(&mut zone, &setting);
                skip_output = false;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            prev_was_count = false;
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let is_count = word == "count";
        match word {
            "probe" | "mask" => {
                // The mask is a constant inside ASSS's Filter, so the
                // transcript recovers it by filtering a one-character word,
                // which never wraps. Do the same and compare: it checks the
                // table and the (5 * count) % len offset in one go.
                if word == "mask" {
                    let (want, tail) = bracketed(rest).expect("mask record");
                    let len: usize = tail.trim().parse().expect("mask length");
                    assert_eq!(want.len(), len, "the record disagrees with itself");
                    set_obscene(&mut zone, "x");
                    let before = filter.replace_count();
                    filter.set_replace_count(0);
                    let mut got = vec![b'?'; len];
                    for k in 0..len {
                        let mut line = "x".to_owned();
                        assert!(filter.filter(&zone, &mut line), "probe {k} did not match");
                        got[(5 * k) % len] = line.as_bytes()[0];
                    }
                    assert_eq!(
                        String::from_utf8_lossy(&got),
                        want,
                        "the replacement mask differs from ASSS's"
                    );
                    filter.set_replace_count(before);
                    checked += 1;
                }
            }
            "words" => {
                let (n, mut tail) = rest.split_once(' ').unwrap_or((rest, ""));
                let n: usize = n.trim().parse().expect("word count");
                let mut want: Vec<String> = Vec::new();
                while let Some((w, next)) = first_bracketed(tail) {
                    want.push(w);
                    tail = next;
                }
                assert_eq!(want.len(), n, "the record disagrees with itself");
                let setting = zone
                    .cfg
                    .global()
                    .get("Chat", "Obscene")
                    .unwrap_or("")
                    .to_owned();
                let got = obscene::parse_words(&setting);
                assert_eq!(got, want, "{label}: parsed word list");
                checked += 1;
            }
            "count" => {
                let want: usize = rest.trim().parse().expect("counter");
                if prev_was_count && filter.replace_count() != want {
                    // The transcript's harness winds the counter to a chosen
                    // offset here, and records both the old value and the
                    // new one. The record before this was the assertion.
                    filter.set_replace_count(want);
                } else {
                    assert_eq!(filter.replace_count(), want, "{label}: replace_count");
                    checked += 1;
                }
            }
            "offsets" => {
                // Only present for the scenario that drives ASSS past the
                // end of its mask. leftover == masklen + 1 reads the mask's
                // own NUL and writes it into the player's line, truncating
                // the message there. We wrap with a modulo instead, so that
                // record's output is deliberately not reproduced -- see the
                // transcript's own note, which asks for exactly this.
                let f = |k: &str| -> usize {
                    rest.split_whitespace()
                        .find_map(|w| w.strip_prefix(k)?.parse().ok())
                        .unwrap_or(0)
                };
                skip_output = f("leftover=") > f("masklen=");
            }
            "filter" => {
                let (input, tail) = first_bracketed(rest).expect("filter input");
                let (flag, tail) = tail.trim().split_once(' ').unwrap_or((tail.trim(), ""));
                let (want_out, counts) = first_bracketed(tail).expect("filter output");
                let v: Vec<usize> = counts
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                assert_eq!(filter.replace_count(), v[0], "{label}: counter before");
                let mut got = input.clone();
                let hit = filter.filter(&zone, &mut got);
                assert_eq!(hit, flag == "1", "{label}: filtered flag for {input:?}");
                if skip_output {
                    // the counter still has to move the same way
                    assert_eq!(filter.replace_count(), v[1], "{label}: counter after");
                    skip_output = false;
                    continue;
                }
                assert_eq!(got, want_out, "{label}: filtering {input:?}");
                assert_eq!(filter.replace_count(), v[1], "{label}: counter after");
                checked += 2;
            }
            _ => {}
        }
        prev_was_count = is_count;
    }
    assert!(checked > 60, "only {checked} records checked");
}

#[test]
fn a_bare_question_mark_does_not_wedge_the_chat_path() {
    // `Chat:Obscene = ?` puts the empty string in ASSS's word list, and its
    // strcasestr returns the haystack for an empty needle, so Filter spins
    // forever on any non-empty line while holding the word-list lock. The
    // transcript cannot record an infinite loop, so this is checked here
    // instead: an empty word is skipped, nothing matches, and the counter
    // does not move.
    let zone = zone_with("?");
    let filter = zone.iface::<dyn Obscene>().expect("obscene");
    assert_eq!(obscene::parse_words("?"), vec![String::new()]);
    let mut line = "a line that must come back untouched".to_owned();
    assert!(!filter.filter(&zone, &mut line));
    assert_eq!(line, "a line that must come back untouched");
    assert_eq!(filter.replace_count(), 0);

    // and the same with a real word beside it, so the list is not empty
    let zone = zone_with("? bad");
    let filter = zone.iface::<dyn Obscene>().expect("obscene");
    let mut line = "bad".to_owned();
    assert!(filter.filter(&zone, &mut line));
    assert_eq!(line, "%@$");
}
