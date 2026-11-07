//! Golden test: the settings packet, prize weights, checksum and override keys
//! built from `fixtures/clientset/synthetic.conf` must match what ASSS 1.4.4's
//! own `clientset.c` code produces for the same values (`golden.txt`, generated
//! by `tools/clientset-oracle`). The synthetic config sets every client setting
//! to a pseudo-random value, including negative, overflowing, hex, octal,
//! boolean and garbage strings, so truncation and `strtol` quirks are covered.

use oss_server::clientset::{self, OverrideKey};
use oss_server::config::ConfigFile;

const CONF: &str = include_str!("fixtures/clientset/synthetic.conf");
const GOLDEN: &str = include_str!("fixtures/clientset/golden.txt");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn settings_match_asss() {
    let cfg = ConfigFile::from_text(CONF);
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
    let s = clientset::load(&cfg);
    let mut checked = 0;
    for line in GOLDEN.lines() {
        let mut f = line.split_whitespace();
        match f.next() {
            Some("settings") => {
                assert_eq!(hex(s.block.as_bytes()), f.next().unwrap_or_default());
                checked += 1;
            }
            Some("checksum") => {
                let key = u32::from_str_radix(f.next().unwrap_or_default(), 16).expect("key");
                let sum = u32::from_str_radix(f.next().unwrap_or_default(), 16).expect("sum");
                assert_eq!(s.block.checksum(key), sum);
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
