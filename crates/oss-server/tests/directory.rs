//! Directory registration: the datagram against PRIVATE's ASSS-compiled
//! vectors, and the module sending it to a local stand-in directory server
//! (never a real one).

use std::net::UdpSocket;
use std::time::Duration;

use oss_proto::directory::ZoneUpdate;
use oss_server::config::MemSource;
use oss_server::modules::directory::update_packet;
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;

const VECTORS: &str = include_str!("fixtures/asss-vectors/directory.txt");

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).expect("hex"))
        .collect()
}

#[test]
fn packets_match_asss() {
    // the cases h_directory.c generates, in order
    #[allow(clippy::type_complexity)]
    let cases: [(Option<&str>, Option<&str>, Option<&str>, u16, u16); 7] = [
        (None, None, None, 5000, 0),
        (Some("Test Zone"), Some("a small test zone"), None, 5000, 17),
        (
            Some("Test Zone"),
            Some("a small test zone"),
            Some("secret"),
            5000,
            17,
        ),
        (Some("Zone"), Some(""), Some("cane"), 1, 65535),
        (
            Some("0123456789012345678901234567890123"),
            Some("d"),
            Some("p"),
            65535,
            1,
        ),
        (
            Some("Zone"),
            Some("0123456789012345678901234567890123456789"),
            Some("0123456789012345678"),
            7000,
            3,
        ),
        (
            Some("Virtual"),
            Some("second listen socket"),
            Some("pw"),
            5002,
            9,
        ),
    ];
    let vectors: Vec<(usize, Vec<u8>)> = VECTORS
        .lines()
        .filter(|l| l.starts_with("update "))
        .map(|l| {
            let mut f = l.split_whitespace().skip(1);
            let n = f.next().and_then(|n| n.parse().ok()).expect("len");
            (n, unhex(f.next().expect("hex")))
        })
        .collect();
    assert_eq!(vectors.len(), cases.len());
    for ((name, desc, pw, port, players), (len, bytes)) in cases.iter().zip(&vectors) {
        let got = update_packet(*port, *players, *name, *desc, *pw);
        assert_eq!(got.len(), *len);
        assert_eq!(&got, bytes, "{name:?}");
    }
    // a description longer than ASSS's field is cut to 385 bytes + NUL
    let long = "x".repeat(500);
    assert_eq!(update_packet(1, 0, None, Some(&long), None).len(), 94 + 386);
}

#[test]
fn registers_with_a_local_directory() {
    let dir = UdpSocket::bind("127.0.0.1:0").expect("bind");
    dir.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let global = format!(
        "[Listen]\nPort = 5123\n[Directory]\nName = Local Test\nDescription = hello\n\
         Server1 = 127.0.0.1\nPort1 = {}\n",
        dir.local_addr().expect("addr").port()
    );
    let src = MemSource::new([
        ("conf/global.conf", global),
        ("arenas/(default)/arena.conf", String::new()),
    ]);
    let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    modules.retain(|m| m != "auth_file");
    modules.push("directory".into());
    let mut zone = ServerBuilder::with_source(src)
        .modules(modules)
        .build_zone()
        .expect("zone");
    // first update after 10 seconds
    for t in 1..=1000 {
        zone.set_now(Ticks(t));
        oss_server::timers::run_due(&mut zone);
    }
    let mut buf = [0u8; 600];
    let (n, from) = dir.recv_from(&mut buf).expect("registration");
    let u = ZoneUpdate::decode(&buf[..n]).expect("decode");
    assert!(u.is_acceptable());
    assert_eq!((u.port, u.players, u.version), (5123, 0, 134));
    assert_eq!(u.name.as_bytes(), b"Local Test");
    assert_eq!(u.description, b"hello");
    // sent from an ephemeral port, not the game port or game port + 1
    assert!(from.port() != 5123 && from.port() != 5124);
}
