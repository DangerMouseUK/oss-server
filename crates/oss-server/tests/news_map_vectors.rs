//! News and map downloads replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/news-map.txt`, from `core/mapnewsdl.c` compiled
//! unmodified).
//!
//! The transcript states its own limit up front: its harness does not link
//! zlib, so a compressed payload is a real zlib stream of stored blocks and
//! its *length* is the harness's, not zlib's. So the compressed records are
//! checked for structure — the header, the CRC-32 of the uncompressed file,
//! and that the payload inflates back to the input — while the `docomp=0`
//! records, which have no compressor in them at all, are compared byte for
//! byte.

use std::io::Read;

use oss_server::config::MemSource;
use oss_server::modules::mapnewsdl::load_download;

const VECTORS: &str = include_str!("fixtures/asss-vectors/news-map.txt");

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn field(rest: &str, key: &str) -> Option<String> {
    rest.split_whitespace()
        .find_map(|w| w.strip_prefix(key).map(str::to_owned))
}

/// `path=[...]`, which may hold spaces or brackets of its own.
fn bracketed(rest: &str, key: &str) -> Option<String> {
    let at = rest.find(key)? + key.len();
    let tail = &rest[at..];
    let open = tail.find('[')?;
    let close = tail[open..].find(']')? + open;
    Some(tail[open + 1..close].to_owned())
}

fn inflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .expect("a valid zlib stream");
    out
}

#[test]
fn news_and_map_downloads_match_asss() {
    let mut input: Vec<u8> = Vec::new();
    let mut checked = 0usize;
    let mut exact = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "input" => {
                let (_len, hexed) = rest.split_once(' ').unwrap_or((rest, "-"));
                input = unhex(hexed.trim());
            }
            "news" => {
                // the news file is served from the same place a map is, with
                // an empty name: sixteen zero bytes in the packet
                let pkt = oss_server::modules::mapnewsdl::news_packet(&input);
                let want_crc = field(rest, "checksum=").expect("checksum");
                assert_eq!(
                    format!("{:08x}", oss_proto::checksum::crc32(&input)),
                    want_crc,
                    "the news checksum is the CRC-32 of the uncompressed file"
                );
                let want_hdr = field(rest, "hdr=").expect("hdr");
                assert_eq!(hex(&pkt[..17]), want_hdr, "news header");
                assert_eq!(inflate(&pkt[17..]), input, "news payload inflates back");
                checked += 3;
            }
            "map" => {
                let path = bracketed(rest, "path=").expect("path");
                let docomp = field(rest, "docomp=").as_deref() == Some("1");
                let want_name = bracketed(rest, "filename=").expect("filename");
                let want_crc = field(rest, "checksum=").expect("checksum");
                let want_hdr = field(rest, "hdr=").expect("hdr");
                let want_payload = field(rest, "payload=").expect("payload");
                // MemSource looks a path up with backslashes turned into
                // forward ones, so store it under that key; the path handed
                // to load_download keeps its original spelling, which is
                // what the basename rule is about
                let key = path.replace('\\', "/");
                let src = MemSource::new([(key, input.clone())]);
                let d = load_download(&src, std::path::Path::new(&path), docomp, false)
                    .expect("map download");
                assert_eq!(d.name, want_name, "stored basename for {path}");
                assert_eq!(format!("{:08x}", d.checksum), want_crc, "map CRC-32");
                assert_eq!(d.raw_len, input.len(), "uncompressed length");
                assert_eq!(hex(&d.packet[..17]), want_hdr, "packet header for {path}");
                if docomp {
                    // the payload's length is the transcript harness's, not
                    // zlib's; what has to hold is that it inflates back
                    assert_eq!(inflate(&d.packet[17..]), input, "map payload inflates back");
                } else {
                    assert_eq!(hex(&d.packet[17..]), want_payload, "verbatim payload");
                    assert_eq!(
                        d.packet.len(),
                        input.len() + 17,
                        "an uncompressed packet is exactly the file plus the header"
                    );
                    exact += 1;
                }
                checked += 5;
            }
            // These pin RefreshNewsTxt's caching: a lazy first read, a
            // re-read when the mtime moves, and no re-read when it does
            // not. Only the first has its input in the file -- the others
            // follow a file change the transcript does not record -- so
            // there is nothing here to replay them against.
            "lazy" | "reread" | "skipped" => {}
            _ => {}
        }
    }
    assert!(exact >= 5, "only {exact} records compared byte for byte");
    assert!(checked > 40, "only {checked} records checked");
}
