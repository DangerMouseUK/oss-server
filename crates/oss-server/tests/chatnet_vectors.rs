//! chatnet's line framing, dispatch and outbound formatting, replayed
//! against ASSS's own transcript (`fixtures/asss-vectors/chatnet.txt`, from
//! `core/chatnet.c` and `main/protutil.c` compiled unmodified).
//!
//! The transcript opens no socket, and neither does this: the framing is in
//! `LineBuffer` and the formatting in `out_line`, and neither knows what a
//! `TcpStream` is. That separation is what made the file replayable at all.
//!
//! What the transcript pins and this does not replay: the per-player
//! filtering behind `SendToOne` and `SendToArena` (its rule 8), which is
//! module behaviour needing a zone, a status and an arena. Its one
//! divergence -- that a game client must be skipped rather than handed a
//! line -- is now a check inside `send_line`; the rest of that rule is
//! exercised over a real socket by `chatnet_flow.rs`.

use std::collections::VecDeque;

use oss_server::modules::chatnet::{LineBuffer, MAX_LINE, out_line, split_command};

const VECTORS: &str = include_str!("fixtures/asss-vectors/chatnet.txt");

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    assert!(s.len().is_multiple_of(2), "odd hex run: {s}");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The text between the first `[` and the last `]` of a record. `<null>`
/// is the transcript's way of saying the module passed a null rest rather
/// than an empty string; `process_line` turns one into the other before a
/// handler sees it, so both arrive here as the empty string.
fn bracketed(rest: &str) -> String {
    let inner = rest
        .split_once('[')
        .and_then(|(_, r)| r.rsplit_once(']'))
        .map_or("", |(a, _)| a);
    if inner == "<null>" {
        String::new()
    } else {
        inner.to_owned()
    }
}

#[test]
fn chatnet_replays_the_asss_transcript() {
    // one buffer per seat
    let mut bufs: Vec<LineBuffer> = Vec::new();
    // seat kinds, as the transcript spells them: 'c' chat, 'v' VIE game,
    // 'o' chat in another arena, 'n' chat not yet playing
    let mut seats: Vec<char> = Vec::new();
    // tag -> command, in registration order
    let mut handlers: Vec<(String, String)> = Vec::new();
    // handler calls the last `process` should produce, still unclaimed
    let mut expected: VecDeque<(String, String)> = VecDeque::new();
    // bytes the last `sendone`/`sendarena` should put on a wire
    let mut outbound: Option<Vec<u8>> = None;
    let mut records = 0usize;
    let (mut feeds, mut procs, mut calls, mut outs) = (0usize, 0usize, 0usize, 0usize);

    for raw in VECTORS.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let v: Vec<&str> = rest.split_whitespace().collect();
        if word != "call" {
            assert!(
                expected.is_empty(),
                "handlers that should have fired did not: {expected:?}"
            );
        }
        records += 1;
        match word {
            "sizes" => {
                assert_eq!(
                    rest.split_whitespace().next(),
                    Some(format!("MAXMSGSIZE={MAX_LINE}").as_str()),
                    "the protocol's line size moved"
                );
            }
            "seat" => {
                seats = v[0].chars().collect();
                bufs = seats.iter().map(|_| LineBuffer::default()).collect();
            }
            "handler" => handlers.push((v[1].to_owned(), v[0].to_owned())),
            "feed" => {
                let who: usize = v[0].parse().expect("player");
                let offered: usize = v[1].parse().expect("offered");
                let taken: usize = v[2].parse().expect("taken");
                let fitted = unhex(v[3]);
                assert_eq!(fitted.len(), taken, "the record disagrees with itself");
                // the transcript gives only the bytes that FITTED. Where
                // more was offered it is a run of one padding byte, so the
                // rest can be reconstructed -- checked, not assumed.
                let mut all = fitted.clone();
                if offered > taken {
                    let pad = fitted[0];
                    assert!(
                        fitted.iter().all(|b| *b == pad),
                        "a short feed whose bytes are not one run cannot be padded back"
                    );
                    all.resize(offered, pad);
                }
                assert_eq!(
                    bufs[who].feed(&all),
                    taken,
                    "p{who}: bytes accepted out of a {offered}-byte read"
                );
                feeds += 1;
            }
            "inbuf" => {
                let who: usize = v[0].parse().expect("player");
                // `- -` is ASSS having no buffer at all rather than an
                // empty one; nothing downstream can tell the two apart
                let want = unhex(v[2]);
                assert_eq!(
                    want.len(),
                    v[1].parse().unwrap_or(0),
                    "the record disagrees with itself"
                );
                assert_eq!(
                    hex(bufs[who].pending()),
                    hex(&want),
                    "p{who}: what is left buffered"
                );
            }
            "process" => {
                let who: usize = v[0].parse().expect("player");
                if let Some(l) = bufs[who].process() {
                    let (cmd, rest) = split_command(&l);
                    let rest = String::from_utf8_lossy(&rest).into_owned();
                    // every handler registered for the command fires, in
                    // registration order; an unknown command fires none.
                    // The fold is here rather than assumed of `cmd` so
                    // that this does not pin HOW the case stops mattering;
                    // that it does is checked through the real dispatch by
                    // `chatnet_flow.rs`.
                    for (tag, for_cmd) in &handlers {
                        if for_cmd.eq_ignore_ascii_case(&cmd) {
                            expected.push_back((tag.clone(), rest.clone()));
                        }
                    }
                }
                procs += 1;
            }
            "call" => {
                let (tag, want) = (v[0], bracketed(rest));
                let (got_tag, got_rest) = expected
                    .pop_front()
                    .unwrap_or_else(|| panic!("no handler fired for: {line}"));
                assert_eq!(got_tag, tag, "the wrong handler, or the wrong order");
                assert_eq!(got_rest, want, "the rest handed to {tag}");
                calls += 1;
            }
            "sendone" => {
                let who: usize = v[0].parse().expect("player");
                // SendToOne skips a game client and nobody else
                outbound = (seats[who] != 'v').then(|| out_line(&bracketed(rest)));
            }
            "sendarena" => outbound = Some(out_line(&bracketed(rest))),
            "to" => {}
            "out" => {
                let n: usize = v[0].parse().expect("length");
                let want = unhex(v[1]);
                assert_eq!(want.len(), n, "the record disagrees with itself");
                let got = outbound
                    .clone()
                    .unwrap_or_else(|| panic!("a line went out that should not have: {line}"));
                assert_eq!(hex(&got), hex(&want), "the bytes of an outbound line");
                outs += 1;
            }
            _ => panic!("unknown record: {line}"),
        }
    }
    assert!(expected.is_empty(), "unclaimed handler calls: {expected:?}");
    assert!(
        records > 70 && feeds >= 10 && procs >= 15 && calls >= 12 && outs >= 6,
        "too little replayed: {records} records, {feeds} feeds, {procs} processes, \
         {calls} calls, {outs} lines out"
    );
}

#[test]
fn an_over_long_line_is_discarded_whole() {
    // The transcript's own over-long scenario ends with the buffer holding
    // a terminator, so it cannot show the discard itself -- the giant run
    // is dispatched as one unknown command instead, which empties the
    // buffer the same way. This is the other half: a buffer filled with no
    // terminator anywhere, which can never complete and is thrown away.
    let mut b = LineBuffer::default();
    assert_eq!(b.feed(&vec![b'x'; 4096]), MAX_LINE - 1, "the buffer's size");
    assert_eq!(
        b.feed(b"\n"),
        0,
        "a full buffer takes nothing, not even a newline"
    );
    assert!(b.process().is_none(), "there is no line to take");
    assert!(
        b.is_empty(),
        "the whole buffer goes, rather than being truncated"
    );
    // and the connection carries on from there
    assert_eq!(b.feed(b"GO:after\n"), 9);
    assert_eq!(b.process().as_deref(), Some(&b"GO:after"[..]));
}
