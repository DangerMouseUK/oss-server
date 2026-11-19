//! Golden test: the settings packet, prize weights, checksum and override keys
//! built from `fixtures/clientset/synthetic.conf` must match what ASSS 1.4.4's
//! own `clientset.c` code produces for the same values (`golden.txt`, generated
//! by `tools/clientset-oracle`). The synthetic config sets every client setting
//! to a pseudo-random value, including negative, overflowing, hex, octal,
//! boolean and garbage strings, so truncation and `strtol` quirks are covered.
//!
//! One deliberate difference: a setting that crashes Continuum at 0
//! ([`clientset::ZERO_CRASHES_CONTINUUM`]) is never sent as 0. Where the
//! synthetic values leave one at 0, ASSS's bytes are compared with that one
//! setting replaced by its safe value, and the checksums are checked against
//! ASSS's own bytes.

use oss_server::clientset::{self, OverrideKey, SETTINGS_LEN, SettingsBlock};
use oss_server::config::ConfigFile;

const CONF: &str = include_str!("fixtures/clientset/synthetic.conf");
const GOLDEN: &str = include_str!("fixtures/clientset/golden.txt");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> [u8; SETTINGS_LEN] {
    let mut out = [0u8; SETTINGS_LEN];
    assert_eq!(text.len(), 2 * SETTINGS_LEN, "golden packet length");
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex");
    }
    out
}

/// ASSS's packet, with each setting that crashes Continuum at 0 replaced by
/// its safe value when ASSS left it at 0; and how many were replaced.
fn as_we_send_it(asss: [u8; SETTINGS_LEN]) -> ([u8; SETTINGS_LEN], usize) {
    let mut block = SettingsBlock::from_bytes(asss);
    let mut replaced = 0;
    for (name, safe) in clientset::ZERO_CRASHES_CONTINUUM {
        let (sec, key) = name.split_once(':').expect("name");
        let k = clientset::override_key(sec, key).expect("override key");
        if block.get(k) == 0 {
            let mut o = clientset::Overrides::default();
            assert!(o.set(k, i32::from(safe)));
            block = block.with_overrides(&o, None);
            replaced += 1;
        }
    }
    (*block.as_bytes(), replaced)
}

#[test]
fn settings_match_asss() {
    let cfg = ConfigFile::from_text(CONF);
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
    let s = clientset::load(&cfg);
    let asss = GOLDEN
        .lines()
        .find_map(|l| l.strip_prefix("settings "))
        .map(|h| SettingsBlock::from_bytes(unhex(h.trim())))
        .expect("golden settings");
    let mut checked = 0;
    for line in GOLDEN.lines() {
        let mut f = line.split_whitespace();
        match f.next() {
            Some("settings") => {
                let (want, replaced) = as_we_send_it(unhex(f.next().unwrap_or_default()));
                // the synthetic values leave exactly one of the three at 0;
                // if the fixture is regenerated, this says the comparison moved
                assert_eq!(replaced, 1, "crash-at-zero settings left at 0 by ASSS");
                assert_eq!(hex(s.block.as_bytes()), hex(&want));
                checked += 1;
            }
            Some("checksum") => {
                let key = u32::from_str_radix(f.next().unwrap_or_default(), 16).expect("key");
                let sum = u32::from_str_radix(f.next().unwrap_or_default(), 16).expect("sum");
                // the algorithm, against ASSS's own bytes
                assert_eq!(asss.checksum(key), sum);
                checked += 1;
            }
            Some("pwps") => {
                let want: Vec<u16> = f.map(|x| x.parse().expect("pwps")).collect();
                assert_eq!(&s.prize_weights.sums()[..], &want[..]);
                checked += 1;
            }
            Some("key") => {
                let name = f.next().unwrap_or_default();
                let want = u32::from_str_radix(f.next().unwrap_or_default(), 16).expect("hex");
                let (sec, key) = name.split_once(':').expect("name");
                let got = clientset::override_key(sec, key).map_or(0, |OverrideKey(k)| k);
                assert_eq!(got, want, "override key for {name}");
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(
        checked > 800,
        "golden file looks truncated ({checked} checks)"
    );
}

/// The packet decodes with `oss-proto`'s client-side codec, re-encodes to the
/// same bytes, and both crates agree on the security checksum.
#[test]
fn settings_round_trip_through_oss_proto() {
    let s = clientset::load(&ConfigFile::from_text(CONF));
    let bytes = s.block.as_bytes();
    let decoded = oss_proto::ClientSettings::decode(bytes).expect("decode");
    assert_eq!(decoded.to_bytes(), bytes.to_vec());
    for key in [0, 1, 0x9e37_79b9, u32::MAX] {
        assert_eq!(
            s.block.checksum(key),
            oss_proto::checksum::settings_checksum(key, bytes)
        );
    }
}
