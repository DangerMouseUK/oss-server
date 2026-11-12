//! File transfer: the server sending a file to a client (`?getfile`),
//! taking one from a client (`?putfile`, `?putzip`, `?putmap`), the working
//! directory commands, and one player offering another a file.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Unique per call, not per clock reading. The tests here run as threads of
/// one process, so the pid is shared and `as_nanos` is only as fine as the
/// platform clock; two tests that agreed on a name would share a directory,
/// and the first to finish would delete it under the others. Nine tests use
/// this helper.
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_zone(files: &[(&str, &str)]) -> TempDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("oss-ft-{}-{nanos}-{n}", std::process::id()));
    // create_dir, not create_dir_all: a name that is somehow taken should
    // fail here rather than quietly share someone else's files
    std::fs::create_dir(&dir).expect("a directory of this name already exists");
    let owned = TempDir(dir);
    for (path, data) in files {
        let p = owned.0.join(path);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, data).expect("write");
    }
    owned
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new(dir: &TempDir) -> Self {
        let zone = ServerBuilder::new(&dir.0).build_zone().expect("zone");
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
        }
    }

    fn tick(&mut self, n: u64) {
        for _ in 0..n {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
    }

    fn player(&mut self, name: &str) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
            machine_id: 1,
            connect_type: 4,
            timezone_bias: 0,
            unknown1: 0,
            client_version: 134,
            field444: 444,
            bound_ip: 0,
            permission_id: 1,
            server_ip: 0,
            bound_port: 0,
            redirect_id: 0,
            using_proxy: 0,
            unused: 0,
            cont_id: None,
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        p
    }

    fn send(&mut self, p: PlayerId, data: &[u8]) {
        self.zone.handle_packet(p, data);
        self.lo.collect(&mut self.zone);
    }

    /// Types a command and returns everything the player was sent after it,
    /// including sized transfers.
    fn cmd(&mut self, p: PlayerId, text: &str) -> Vec<Vec<u8>> {
        self.lo.take(p);
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind: chat_type::PUBLIC,
            sound: 0,
            pid: 0,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.send(p, &buf);
        self.packets(p)
    }

    /// Everything the player was sent, including sized transfers.
    fn packets(&mut self, p: PlayerId) -> Vec<Vec<u8>> {
        self.lo.take(p).into_iter().map(|(d, _)| d).collect()
    }
}

const CONF: &[(&str, &str)] = &[
    ("conf/global.conf", "[Chat]\nMessageReliable = 1\n"),
    ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
    (
        "conf/groupdef.conf",
        "[default]\ncmd_cd = 1\ncmd_pwd = 1\ncmd_getfile = 1\ncmd_putfile = 1\ncmd_delfile = 1\n\
         cmd_renfile = 1\ncmd_putzip = 1\ncmd_putmap = 1\ncmd_makearena = 1\nuploadfile\n\
         privcmd_sendfile = 1\ncmd_acceptfile = 1\ncmd_cancelfile = 1\n",
    ),
    ("conf/staff.conf", ""),
    ("arenas/(default)/arena.conf", "[Misc]\nA = 1\n"),
];

/// An upload from a client: C2S 0x16 with a 16-byte name, then the file.
fn upload(h: &mut H, p: PlayerId, data: &[u8]) {
    let mut buf = Vec::new();
    c2s::UploadFile {
        name: FixedStr::new(b"whatever"),
        data: data.to_vec(),
    }
    .encode(&mut buf);
    h.send(p, &buf);
}

/// The chat lines among some packets.
fn chats(pkts: &[Vec<u8>]) -> Vec<String> {
    pkts.iter()
        .filter(|d| d[0] == s2c::kind::CHAT)
        .map(|d| String::from_utf8_lossy(&S2CChat::decode(d).expect("chat").0.text).into_owned())
        .collect()
}

/// The file in an S2C 0x10 among some packets, if any.
fn incoming_file(pkts: &[Vec<u8>]) -> Option<(String, Vec<u8>)> {
    pkts.iter()
        .find(|d| d[0] == s2c::kind::INCOMING_FILE)
        .and_then(|d| s2c::IncomingFile::decode(d).ok())
        .map(|f| {
            (
                String::from_utf8_lossy(f.name.as_bytes()).into_owned(),
                f.data,
            )
        })
}

#[test]
fn getfile_sends_a_file_from_the_zone() {
    let mut files = CONF.to_vec();
    files.push(("data/notes.txt", "hello from the zone"));
    let dir = temp_zone(&files);
    let mut h = H::new(&dir);
    let p = h.player("alice");

    let out = chats(&h.cmd(p, "?pwd"));
    assert!(
        out.iter().any(|t| t == "Current working directory: ."),
        "{out:?}"
    );
    let out = chats(&h.cmd(p, "?cd data"));
    assert!(
        out.iter().any(|t| t == "Changed working directory."),
        "{out:?}"
    );
    let out = chats(&h.cmd(p, "?pwd"));
    assert!(
        out.iter().any(|t| t == "Current working directory: data"),
        "{out:?}"
    );

    let got = h.cmd(p, "?getfile notes.txt");
    let (name, data) = incoming_file(&got).expect("a file");
    assert_eq!(name, "notes.txt");
    assert_eq!(data, b"hello from the zone");
}

#[test]
fn a_path_cannot_climb_out_of_the_zone() {
    let dir = temp_zone(CONF);
    // something to steal, next to the zone rather than inside it
    let outside = dir.0.join("..").join("oss-ft-secret.txt");
    std::fs::write(&outside, "not yours").expect("write");
    let mut h = H::new(&dir);
    let p = h.player("alice");

    for path in ["../oss-ft-secret.txt", "/etc/passwd", "data/../../x"] {
        let got = h.cmd(p, &format!("?getfile {path}"));
        let out = chats(&got);
        assert!(
            out.iter().any(|t| t == "Invalid path."),
            "{path} was not refused: {out:?}"
        );
        assert!(incoming_file(&got).is_none(), "{path} sent a file");
    }
    // and ?cd can't either
    let out = chats(&h.cmd(p, "?cd .."));
    assert!(
        out.iter()
            .any(|t| t == "Invalid path, or it isn't a directory in the zone."),
        "{out:?}"
    );
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn putfile_takes_an_upload_and_delfile_removes_it() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let p = h.player("alice");

    // the server asks the client for the file (S2C 0x19)
    // the colon separates the two names, as in ASSS, so the client path
    // can't have one of its own
    let got = h.cmd(p, "?putfile maps\\mine.lvl:data/mine.lvl");
    let asked = got
        .into_iter()
        .find(|d| d[0] == s2c::kind::REQUEST_FILE)
        .expect("0x19");
    let asked = s2c::RequestFile::decode(&asked).expect("decode");
    assert_eq!(
        String::from_utf8_lossy(asked.local_path.as_bytes()),
        "maps\\mine.lvl"
    );

    upload(&mut h, p, b"a map, honest");
    let out = chats(&h.packets(p));
    assert!(
        out.iter().any(|t| t.starts_with("File received:")),
        "{out:?}"
    );
    let landed = dir.0.join("data/mine.lvl");
    assert_eq!(
        std::fs::read(&landed).ok().as_deref(),
        Some(b"a map, honest".as_slice())
    );
    // nothing is left behind in tmp/
    let left: Vec<_> = std::fs::read_dir(dir.0.join("tmp"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");

    // ?renfile then ?delfile
    let out = chats(&h.cmd(p, "?renfile data/mine.lvl:data/yours.lvl"));
    assert!(out.iter().any(|t| t == "Renamed."), "{out:?}");
    assert!(dir.0.join("data/yours.lvl").exists());
    let out = chats(&h.cmd(p, "?delfile data/yours.lvl"));
    assert!(out.iter().any(|t| t == "Deleted."), "{out:?}");
    assert!(!dir.0.join("data/yours.lvl").exists());
}

#[test]
fn an_upload_nobody_asked_for_is_dropped() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let p = h.player("alice");
    upload(&mut h, p, b"surprise");
    let tmp = dir.0.join("tmp");
    let left: Vec<_> = std::fs::read_dir(&tmp)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "an unasked-for upload was kept: {left:?}");
}

/// A zip holding two files, one of them inside a directory.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, body) in entries {
        let mut deflated = Vec::new();
        {
            let mut e =
                flate2::write::DeflateEncoder::new(&mut deflated, flate2::Compression::fast());
            e.write_all(body).expect("deflate");
            e.finish().expect("deflate");
        }
        let offset = out.len() as u32;
        let (nlen, clen, ulen) = (name.len() as u16, deflated.len() as u32, body.len() as u32);
        out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
        out.extend_from_slice(&[20, 0, 0, 0, 8, 0, 0, 0, 0, 0]); // version, flags, method 8, time
        out.extend_from_slice(&0u32.to_le_bytes()); // crc, which this reader doesn't check
        out.extend_from_slice(&clen.to_le_bytes());
        out.extend_from_slice(&ulen.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&deflated);

        central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&clen.to_le_bytes());
        central.extend_from_slice(&ulen.to_le_bytes());
        central.extend_from_slice(&nlen.to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // extra, comment, disk, attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let (cd_at, cd_len) = (out.len() as u32, central.len() as u32);
    out.extend_from_slice(&central);
    out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn putzip_unpacks_into_one_directory() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let p = h.player("alice");
    h.cmd(p, "?putzip stuff.zip:data");
    // the paths inside the archive are ignored, as `unzip -j` does
    let zip = zip_of(&[
        ("one.txt", b"first".as_slice()),
        ("deep/two.txt", b"second".as_slice()),
        ("../escape.txt", b"nope".as_slice()),
    ]);
    upload(&mut h, p, &zip);
    let out = chats(&h.packets(p));
    assert!(
        out.iter().any(|t| t.starts_with("Zip inflated to")),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read(dir.0.join("data/one.txt")).ok().as_deref(),
        Some(b"first".as_slice())
    );
    assert_eq!(
        std::fs::read(dir.0.join("data/two.txt")).ok().as_deref(),
        Some(b"second".as_slice())
    );
    assert_eq!(
        std::fs::read(dir.0.join("data/escape.txt")).ok().as_deref(),
        Some(b"nope".as_slice()),
        "a path in the archive lands under its own name, not above the directory"
    );
    assert!(!dir.0.join("escape.txt").exists());
    assert!(!dir.0.join("data/deep").exists());
}

#[test]
fn a_malformed_zip_is_refused_without_panicking() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let p = h.player("alice");
    for bad in [
        b"not a zip at all".to_vec(),
        vec![0x50, 0x4b, 0x05, 0x06],
        {
            let mut z = zip_of(&[("x.txt", b"hi")]);
            z.truncate(z.len() / 2);
            z
        },
    ] {
        h.cmd(p, "?putzip c:\\stuff.zip:data");
        upload(&mut h, p, &bad);
        // it either says it couldn't, or writes nothing; never a panic
        let out = chats(&h.packets(p));
        assert!(!out.iter().any(|t| t.contains("inflated to")), "{out:?}");
    }
}

#[test]
fn putmap_sets_the_arena_map() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let p = h.player("alice");
    let arena = h.zone.players.get(p).and_then(|x| x.arena).expect("arena");
    h.cmd(p, "?putmap c:\\maps\\new.lvl");
    upload(&mut h, p, b"a level file");
    let out = chats(&h.packets(p));
    assert!(
        out.iter()
            .any(|t| t == "Set General:Map=maps/uploads/(public).lvl"),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read(dir.0.join("maps/uploads/(public).lvl"))
            .ok()
            .as_deref(),
        Some(b"a level file".as_slice())
    );
    assert_eq!(
        h.zone
            .arenas
            .get(arena)
            .and_then(|a| a.cfg.get("General", "Map")),
        Some("maps/uploads/(public).lvl")
    );
}

#[test]
fn makearena_writes_an_arena_config() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let p = h.player("alice");
    let out = chats(&h.cmd(p, "?makearena Duel"));
    assert!(out.iter().any(|t| t == "Illegal arena name."), "{out:?}");
    let out = chats(&h.cmd(p, "?makearena duel"));
    assert!(
        out.iter().any(|t| t == "Successfully created duel."),
        "{out:?}"
    );
    let conf = std::fs::read_to_string(dir.0.join("arenas/duel/arena.conf")).expect("conf");
    assert!(
        conf.contains("#include arenas/(default)/arena.conf"),
        "{conf}"
    );
}

#[test]
fn one_player_offers_another_a_file() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let alice = h.player("alice");
    let bob = h.player("bob");

    let out = chats(&h.cmd(alice, "?sendfile notes.txt"));
    assert!(out.is_empty(), "without a target it does nothing: {out:?}");

    // alice offers bob a file (a private command aimed at him)
    let mut buf = Vec::new();
    c2s::C2SChat(ChatBody {
        kind: chat_type::PRIVATE,
        sound: 0,
        pid: bob.0 as i16,
        text: b"?sendfile c:\\notes.txt".to_vec(),
    })
    .encode(&mut buf);
    h.lo.take(alice);
    h.lo.take(bob);
    h.send(alice, &buf);
    let to_alice = chats(&h.packets(alice));
    assert!(
        to_alice
            .iter()
            .any(|t| t == "Waiting for bob to accept your file."),
        "{to_alice:?}"
    );
    let to_bob = chats(&h.packets(bob));
    assert!(
        to_bob.iter().any(|t| t
            == "alice wants to send you the file \"c:\\notes.txt\". To accept it, type ?acceptfile."),
        "{to_bob:?}"
    );

    // bob accepts: alice's client is asked for the file
    h.cmd(bob, "?acceptfile");
    let asked = h
        .packets(alice)
        .into_iter()
        .find(|d| d[0] == s2c::kind::REQUEST_FILE);
    assert!(asked.is_some(), "alice's client was asked for the file");

    // alice's client uploads it, and bob gets it under its own name
    upload(&mut h, alice, b"some notes");
    assert_eq!(
        incoming_file(&h.packets(bob)),
        Some(("notes.txt".to_owned(), b"some notes".to_vec()))
    );
    assert!(chats(&h.packets(alice)).iter().any(|t| t == "File sent."));
    // and the server keeps no copy
    let left: Vec<_> = std::fs::read_dir(dir.0.join("tmp"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn an_offer_is_withdrawn_and_only_one_at_a_time() {
    let dir = temp_zone(CONF);
    let mut h = H::new(&dir);
    let alice = h.player("alice");
    let bob = h.player("bob");
    let offer = |h: &mut H, from: PlayerId, to: PlayerId, what: &str| -> Vec<String> {
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind: chat_type::PRIVATE,
            sound: 0,
            pid: to.0 as i16,
            text: format!("?sendfile {what}").into_bytes(),
        })
        .encode(&mut buf);
        h.lo.take(from);
        h.send(from, &buf);
        chats(&h.packets(from))
    };
    offer(&mut h, alice, bob, "c:\\one.txt");
    let out = offer(&mut h, alice, bob, "c:\\two.txt");
    assert!(
        out.iter()
            .any(|t| t.starts_with("You are currently sending a file.")),
        "{out:?}"
    );
    let out = chats(&h.cmd(alice, "?cancelfile"));
    assert!(
        out.iter()
            .any(|t| t == "Your file offers have been cancelled."),
        "{out:?}"
    );
    let out = chats(&h.cmd(bob, "?acceptfile"));
    assert!(
        out.iter()
            .any(|t| t == "Nobody has offered any files to you."),
        "{out:?}"
    );
    // and a second offer works once the first is gone
    let out = offer(&mut h, alice, bob, "c:\\two.txt");
    assert!(
        out.iter()
            .any(|t| t == "Waiting for bob to accept your file."),
        "{out:?}"
    );
}

/// `Path` is used by the helpers above; keep the import honest.
#[allow(dead_code)]
fn _uses_path(p: &Path) -> bool {
    p.exists()
}
