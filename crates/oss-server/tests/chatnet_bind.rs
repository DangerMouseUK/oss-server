//! Where the chat protocol listens. A zone whose game port is local-only
//! must not have a chat port open on every interface.
//!
//! Every socket these tests bind is on loopback with a port the system
//! picks, so nothing here listens on the network.

use std::net::SocketAddr;

use oss_server::config::MemSource;
use oss_server::net::{ListenSpec, Listeners};
use oss_server::runtime::ServerBuilder;
use oss_server::zone::{Zone, ZoneStarted};

fn builder(global: &str) -> ServerBuilder {
    let src = MemSource::new([
        ("conf/global.conf", global),
        ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
        ("conf/staff.conf", ""),
        ("arenas/(default)/arena.conf", ""),
    ]);
    let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    modules.push("chatnet".into());
    ServerBuilder::with_source(src).modules(modules)
}

fn zone(global: &str) -> Zone {
    builder(global).build_zone().expect("zone")
}

fn chat_addr(zone: &Zone) -> SocketAddr {
    oss_server::modules::chatnet::local_addr(zone).expect("chat is listening")
}

/// `[Listen] BindAddress = 127.0.0.1` and no `Net:ChatListen`: chat is on
/// loopback too, not `0.0.0.0`.
#[test]
fn a_local_only_game_port_gets_a_local_only_chat_port() {
    let mut z = zone("[Listen]\nPort = 0\nBindAddress = 127.0.0.1\n");
    z.fire(&ZoneStarted);
    let addr = chat_addr(&z);
    assert!(
        addr.ip().is_loopback(),
        "chat listens on {addr}, not on loopback"
    );
}

/// `--bind 127.0.0.1:<port>` replaces the configured game address with a
/// loopback one; the running server records what it bound in `Listeners`
/// before `ZoneStarted`, and chat follows that rather than the file.
#[test]
fn chat_follows_the_bound_game_socket_not_the_file() {
    // the file alone would say every interface; it is never bound here
    let mut z = zone("[Listen]\nPort = 5000\n");
    z.res.insert(Listeners(vec![ListenSpec {
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        allow_vie: true,
        allow_cont: true,
        connect_as: None,
    }]));
    z.fire(&ZoneStarted);
    let addr = chat_addr(&z);
    assert!(
        addr.ip().is_loopback(),
        "chat listens on {addr}, not on loopback"
    );
}

/// The whole `--bind` path, as the binary takes it: a file that says every
/// interface, [`ServerBuilder::bind`] with a loopback address, and a real
/// [`ServerBuilder::start`]. The test above stands in for the runtime by
/// inserting `Listeners` itself; this one leaves that to the runtime, so it
/// fails if `--bind` ever stops reaching chat.
///
/// `ChatListen = 0` asks for a port the system picks. With no setting at all
/// chat would take the game's port number over TCP, which another process
/// may already hold; the address rule being tested is the same either way.
#[test]
fn bind_on_a_started_server_puts_chat_on_the_same_address() {
    let server = builder("[Listen]\nPort = 5000\n[Net]\nChatListen = 0\n")
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .start()
        .expect("server starts");
    let game = server.game_addr();
    let (tx, rx) = std::sync::mpsc::channel();
    assert!(server.handle().run(move |z| {
        let _ = tx.send(oss_server::modules::chatnet::local_addr(z));
    }));
    let chat = rx.recv_timeout(std::time::Duration::from_secs(10));
    server.stop();
    let chat = chat
        .expect("the game thread answered")
        .expect("chat is listening");
    assert!(game.ip().is_loopback(), "--bind was not applied: {game}");
    assert_eq!(
        chat.ip(),
        game.ip(),
        "chat listens on {chat}, the game on {game}"
    );
}

/// An address set in `Net:ChatListen` is honoured as written. Shown here
/// with loopback under a game port that would otherwise be open; opening
/// chat to the network with `0.0.0.0:<port>` is covered without binding,
/// by the unit test beside `chat_listen_addr`.
#[test]
fn an_explicit_chat_address_is_honoured() {
    let mut z = zone("[Listen]\nPort = 5000\n[Net]\nChatListen = 127.0.0.1:0\n");
    z.fire(&ZoneStarted);
    assert!(chat_addr(&z).ip().is_loopback());
}
