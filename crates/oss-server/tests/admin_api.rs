//! The admin HTTP/WebSocket API on a running server (real sockets, localhost).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use oss_server::config::MemSource;
use oss_server::modules::core::{Action, PlayerAction};
use oss_server::player::PlayerId;
use oss_server::runtime::ServerBuilder;

fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).expect("write");
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("status");
    let body = resp
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_owned();
    (status, body)
}

/// Opens a WebSocket by hand (enough of RFC 6455 for one text frame).
fn ws_connect(addr: SocketAddr, path: &str, token: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    s.write_all(req.as_bytes()).expect("write");
    // read the 101 response headers
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).expect("handshake");
        head.push(b[0]);
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
    s
}

fn ws_text(s: &mut TcpStream) -> String {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).expect("frame");
    assert_eq!(h[0], 0x81, "unfragmented text frame");
    let len = match h[1] & 0x7F {
        126 => {
            let mut l = [0u8; 2];
            s.read_exact(&mut l).expect("len");
            usize::from(u16::from_be_bytes(l))
        }
        n => usize::from(n),
    };
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).expect("payload");
    String::from_utf8(body).expect("utf8")
}

#[test]
fn admin_api() {
    let src = MemSource::new([
        (
            "conf/global.conf",
            "[Admin]\nListen = 127.0.0.1:0\nToken = s3cret\n",
        ),
        ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
        ("arenas/(default)/arena.conf", ""),
    ]);
    let server = ServerBuilder::with_source(src)
        .bind(([127, 0, 0, 1], 0).into())
        .start()
        .expect("server");
    let addr = server.admin_addr().expect("admin api");
    let t = Some("s3cret");

    // auth
    assert_eq!(http(addr, "GET", "/api/status", None, "").0, 401);
    assert_eq!(http(addr, "GET", "/api/status", Some("wrong"), "").0, 401);

    // read-only endpoints
    let (code, body) = http(addr, "GET", "/api/status", t, "");
    assert_eq!(code, 200);
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v["players"], 0);
    assert!(v["modules"].as_u64().is_some_and(|n| n > 10));
    let (code, body) = http(addr, "GET", "/api/modules", t, "");
    assert_eq!(code, 200);
    assert!(body.contains("\"admin\""));
    assert_eq!(http(addr, "GET", "/api/players", t, "").1, "[]");
    std::thread::sleep(Duration::from_millis(100));
    let (code, metrics) = http(addr, "GET", "/metrics", t, "");
    assert_eq!(code, 200);
    for want in [
        "oss_ticks_total ",
        "oss_game_busy_seconds_total ",
        "oss_tick_duration_seconds_bucket{le=\"+Inf\"} ",
        "oss_arenas 0",
        "oss_modules ",
    ] {
        assert!(metrics.contains(want), "missing {want} in\n{metrics}");
    }

    // actions
    assert_eq!(http(addr, "POST", "/api/players/77/kick", t, "").0, 404);
    assert_eq!(
        http(addr, "POST", "/api/message", t, r#"{"text":"hello"}"#).0,
        404,
        "no arenas yet"
    );
    assert_eq!(
        http(addr, "POST", "/api/message", t, r#"{"nope":1}"#).0,
        422
    );
    let (code, body) = http(addr, "POST", "/api/reload", t, "");
    assert_eq!((code, body.as_str()), (200, r#"{"files":0}"#));
    assert_eq!(http(addr, "POST", "/api/modules/freqowners", t, "").0, 400);
    assert_eq!(
        http(addr, "DELETE", "/api/modules/freqowners", t, "").0,
        204
    );
    assert!(
        !http(addr, "GET", "/api/modules", t, "")
            .1
            .contains("freqowners")
    );
    assert_eq!(http(addr, "POST", "/api/modules/freqowners", t, "").0, 204);

    // event stream
    let mut ws = ws_connect(addr, "/api/events", "s3cret");
    std::thread::sleep(Duration::from_millis(100));
    server.handle().run(|z| {
        z.fire(&PlayerAction {
            player: PlayerId(3),
            action: Action::Connect,
            arena: None,
        });
    });
    let ev: serde_json::Value = serde_json::from_str(&ws_text(&mut ws)).expect("event");
    assert_eq!(ev["type"], "login");
    assert_eq!(ev["pid"], 3);
    server.handle().run(|z| {
        z.fire(&oss_server::modules::anticheat::CheatSuspected {
            player: PlayerId(3),
            arena: oss_server::arena::ArenaId(0),
            check: oss_server::modules::anticheat::Check::Clock,
            detail: "client clock running 20.0% fast".into(),
            trust: 70,
        });
    });
    let ev: serde_json::Value = serde_json::from_str(&ws_text(&mut ws)).expect("event");
    assert_eq!(
        (
            ev["type"].as_str(),
            ev["check"].as_str(),
            ev["trust"].as_i64()
        ),
        (Some("cheat_suspected"), Some("clock"), Some(70))
    );

    drop(ws);
    assert!(server.stop().is_some());
}
