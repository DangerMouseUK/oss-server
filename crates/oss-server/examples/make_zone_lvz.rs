//! Generates the sample zone's LVZ file, `zone/arenas/scf/scf-demo.lvz`.
//!
//! Run from the repository root:
//!
//! ```text
//! cargo run --example make_zone_lvz
//! ```
//!
//! **Why this exists rather than a borrowed file.** The bundled zone needs
//! to serve LVZ in one arena and not another, so that moving between them
//! exercises a client loading one arena's objects and dropping them again.
//! ASSS's own sample zone ships no `.lvz` — its `; LevelFiles` line names
//! two files it does not include — and the only `.lvz` within reach was
//! another project's, whose images its own build file credits to the
//! Powerball zone and to a named artist. `CLAUDE.md` forbids committing
//! third-party zone content, so the zone's LVZ is made here instead: every
//! byte of it comes from this file, which is short enough to read.
//!
//! The output is deterministic — fixed pixels, a zero file time — so
//! re-running it produces the committed file byte for byte, and a reviewer
//! can check that rather than taking the binary on trust.
//!
//! The objects sit at the middle of the map because that is where a
//! spectator's camera starts: anywhere else and a live run can only count
//! them, not see them.

use std::path::PathBuf;

use oss_formats::lvz::{CLV2, ImageDef, Lvz, ObjectSection, Section};
use oss_proto::objects::{ObjectData, layer, mode};

/// Tile size in pixels, which is what a map object's coordinates are in.
const TILE: i16 = 16;

/// Middle tile of a standard 1024x1024 SubSpace map, which is where a
/// spectator's camera starts. The objects go here so that a live run can
/// see them drawn rather than only count them.
const CENTRE: i16 = 512;

/// A 24-bit uncompressed BMP, `w` by `h`, from a pixel function returning
/// `(r, g, b)`. Rows are stored bottom-up and padded to four bytes, which
/// is the format Continuum reads.
fn bmp(w: u32, h: u32, px: impl Fn(u32, u32) -> (u8, u8, u8)) -> Vec<u8> {
    let row = (w * 3).next_multiple_of(4) as usize;
    let pixels = row * h as usize;
    let mut v = Vec::with_capacity(54 + pixels);
    v.extend_from_slice(b"BM");
    v.extend_from_slice(&(54 + pixels as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // reserved
    v.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
    v.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    v.extend_from_slice(&(w as i32).to_le_bytes());
    v.extend_from_slice(&(h as i32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // planes
    v.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    v.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    v.extend_from_slice(&(pixels as u32).to_le_bytes());
    v.extend_from_slice(&0i32.to_le_bytes()); // x pixels per metre
    v.extend_from_slice(&0i32.to_le_bytes()); // y pixels per metre
    v.extend_from_slice(&0u32.to_le_bytes()); // palette colours
    v.extend_from_slice(&0u32.to_le_bytes()); // important colours
    for y in (0..h).rev() {
        let start = v.len();
        for x in 0..w {
            let (r, g, b) = px(x, y);
            v.extend_from_slice(&[b, g, r]);
        }
        v.resize(start + row, 0);
    }
    v
}

/// A 16x16 hollow square: a border in `edge`, black inside. Black is the
/// transparent colour in Continuum's LVZ images, so this draws as an
/// outline and nothing else.
fn marker(edge: (u8, u8, u8)) -> Vec<u8> {
    bmp(16, 16, |x, y| {
        if x == 0 || y == 0 || x == 15 || y == 15 {
            edge
        } else {
            (0, 0, 0)
        }
    })
}

fn main() {
    let green = marker((0, 220, 0));
    let blue = marker((0, 120, 255));

    // one object shown the moment the arena's LVZ loads, and one the
    // server drives with ?objon / ?objoff, so the file exercises both
    let mut always = ObjectData::map(1, CENTRE * TILE, CENTRE * TILE);
    always.image = 0;
    always.layer = layer::AFTER_TILES;
    always.set_mode(mode::SHOW_ALWAYS);

    let mut driven = ObjectData::map(2, (CENTRE + 2) * TILE, CENTRE * TILE);
    driven.image = 1;
    driven.layer = layer::AFTER_TILES;
    driven.set_mode(mode::SERVER_CONTROLLED);

    let objects = ObjectSection {
        version: CLV2,
        objects: vec![always, driven],
        images: vec![
            ImageDef {
                x_count: 1,
                y_count: 1,
                animation_time: 0,
                name: b"oss-green.bmp".to_vec(),
            },
            ImageDef {
                x_count: 1,
                y_count: 1,
                animation_time: 0,
                name: b"oss-blue.bmp".to_vec(),
            },
        ],
    };

    let lvz = Lvz {
        sections: vec![
            // file time 0 keeps the output reproducible
            Section::new(b"oss-green.bmp", 0, &green),
            Section::new(b"oss-blue.bmp", 0, &blue),
            Section::objects(&objects),
        ],
    };

    let out: PathBuf = ["zone", "arenas", "scf", "scf-demo.lvz"].iter().collect();
    std::fs::write(&out, lvz.to_bytes()).expect("write the LVZ");
    println!("wrote {}", out.display());
}
