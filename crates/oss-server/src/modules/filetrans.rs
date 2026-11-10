//! `filetrans` and `sendfile`: moving files between the server and a game
//! client (ASSS `filetrans.c` and `sendfile.c`).
//!
//! Two directions, both over the transport's sized transfers:
//!
//! - **Down:** [`send_file`] queues S2C `0x10` (file name, then contents) as
//!   a sized transfer, which is how `mapnewsdl` sends maps and news too.
//! - **Up:** [`request_file`] sends S2C `0x19` naming a path on the
//!   *client's* machine; the client answers with C2S `0x16`, which the
//!   transport reassembles. The upload is written to a temporary file under
//!   the zone's `tmp/` directory and handed to the callback that asked for
//!   it, which owns the file from then on. A player may have one upload in
//!   flight, and uploading needs the `uploadfile` capability.
//!
//! Every server-side path is taken relative to the zone directory and must
//! stay inside it ([`resolve`]). ASSS leaves that to the capability system;
//! refusing `..` and absolute paths outright costs the commands nothing and
//! keeps a mistake (or a stolen staff login) inside the zone.
//!
//! `sendfile` is the player-facing half: `?sendfile` offers a file from your
//! own client to another player, who takes it with `?acceptfile`;
//! `?cancelfile` withdraws the offer. Both players have to be spectating, as
//! in ASSS, so nobody uses it mid-game.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use crate::config::to_latin1;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::chat::message;
use super::core::{Action, PlayerAction};

/// Lets a player upload a file to the server (ASSS `CAP_UPLOADFILE`).
pub const CAP_UPLOAD: &str = "uploadfile";

/// Largest file the server will send or accept, so one transfer can't eat
/// the zone's memory. The transport's own incoming limit is the same size.
pub const MAX_FILE: usize = 16 << 20;

/// What to do with a file a client uploaded. `None` means the upload failed
/// or was refused; with a path, the callback owns the file and is expected
/// to move or delete it.
pub type OnUpload = Box<dyn FnOnce(&mut Zone, PlayerId, Option<PathBuf>) + Send>;

#[derive(Default)]
struct Ft {
    /// `?cd`'s working directory, relative to the zone directory.
    work_dir: PathBuf,
    /// Set while an S2C `0x19` is outstanding.
    pending: Option<OnUpload>,
}

fn ft(zone: &mut Zone, p: PlayerId) -> Option<&mut Ft> {
    zone.players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<Ft>())
}

/// The zone's directory, which every server-side path is relative to. A
/// zone without one (tests, an in-memory config) can't transfer files.
pub fn zone_root(zone: &Zone) -> Option<PathBuf> {
    zone.dir.as_ref().map(|d| d.root().to_path_buf())
}

/// Resolves a path given by staff against the zone directory and the
/// player's working directory. `None` when it is absolute, has a prefix, or
/// climbs out of the zone with `..`.
pub fn resolve(zone: &mut Zone, p: PlayerId, path: &str) -> Option<PathBuf> {
    let root = zone_root(zone)?;
    let work = ft(zone, p).map(|f| f.work_dir.clone()).unwrap_or_default();
    let mut rel = PathBuf::new();
    for part in work.components().chain(Path::new(path).components()) {
        match part {
            Component::Normal(c) => rel.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !rel.pop() {
                    return None;
                }
            }
            // absolute paths and Windows prefixes are not ours to touch
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(root.join(rel))
}

/// The working directory shown by `?pwd`, relative to the zone directory.
pub fn working_dir(zone: &mut Zone, p: PlayerId) -> PathBuf {
    ft(zone, p).map(|f| f.work_dir.clone()).unwrap_or_default()
}

/// Sets the working directory (`?cd`). The path is resolved as
/// [`resolve`] does, so it stays inside the zone; false when it doesn't or
/// isn't a directory.
pub fn set_working_dir(zone: &mut Zone, p: PlayerId, path: &str) -> bool {
    let Some(full) = resolve(zone, p, path) else {
        return false;
    };
    if !full.is_dir() {
        return false;
    }
    let Some(root) = zone_root(zone) else {
        return false;
    };
    let rel = full
        .strip_prefix(&root)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    if let Some(f) = ft(zone, p) {
        f.work_dir = rel;
    }
    true
}

/// Sends a file to a player as S2C `0x10` under the name `name` (at most 16
/// bytes, as the packet allows). With `delete_after` the server's copy is
/// removed once the transfer is queued. False if the file can't be read.
pub fn send_file(
    zone: &mut Zone,
    p: PlayerId,
    path: &Path,
    name: &str,
    delete_after: bool,
) -> bool {
    let standard = zone
        .players
        .get(p)
        .is_some_and(|x| matches!(x.kind, ClientKind::Vie | ClientKind::Continuum));
    if !standard {
        return false;
    }
    let data = match std::fs::read(path) {
        Ok(d) if d.len() <= MAX_FILE => d,
        Ok(d) => {
            tracing::warn!(target: "filetrans", file = %path.display(), bytes = d.len(), "too big to send");
            return false;
        }
        Err(e) => {
            tracing::warn!(target: "filetrans", file = %path.display(), "can't read: {e}");
            return false;
        }
    };
    let mut pkt = Vec::with_capacity(17 + data.len());
    pkt.push(s2c::kind::INCOMING_FILE);
    let mut n = to_latin1(name);
    n.resize(16, 0);
    pkt.extend_from_slice(&n);
    pkt.extend_from_slice(&data);
    let bytes: Arc<[u8]> = pkt.into();
    zone.send_sized(p, bytes);
    tracing::info!(target: "filetrans", pid = p.0, file = %path.display(), name, "sending");
    if delete_after && let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(target: "filetrans", file = %path.display(), "can't remove after sending: {e}");
    }
    true
}

/// Asks a client to upload the file at `client_path` on its own machine
/// (S2C `0x19`). `done` runs when the upload arrives, or with `None` if it
/// fails. False when the player already has an upload in flight or isn't a
/// game client.
pub fn request_file(zone: &mut Zone, p: PlayerId, client_path: &str, done: OnUpload) -> bool {
    let standard = zone
        .players
        .get(p)
        .is_some_and(|x| matches!(x.kind, ClientKind::Vie | ClientKind::Continuum));
    if !standard || ft(zone, p).is_none_or(|f| f.pending.is_some()) {
        return false;
    }
    if client_path.contains("..") {
        tracing::warn!(target: "filetrans", pid = p.0, path = client_path, "file request with '..'");
    }
    let mut buf = Vec::new();
    s2c::RequestFile {
        local_path: FixedStr::new(&to_latin1(client_path)),
        name: FixedStr::new(b"unused-field"),
    }
    .encode(&mut buf);
    zone.send(p, &buf, SendFlags::RELIABLE);
    if let Some(f) = ft(zone, p) {
        f.pending = Some(done);
    }
    tracing::info!(target: "filetrans", pid = p.0, path = client_path, "requested");
    true
}

/// Finishes a pending upload: hands the callback the file, or `None`.
fn finish(zone: &mut Zone, p: PlayerId, file: Option<PathBuf>) {
    let Some(done) = ft(zone, p).and_then(|f| f.pending.take()) else {
        // nothing asked for this file; don't keep it
        if let Some(f) = file
            && let Err(e) = std::fs::remove_file(&f)
        {
            tracing::warn!(target: "filetrans", file = %f.display(), "can't remove: {e}");
        }
        return;
    };
    done(zone, p, file);
}

/// A unique name under the zone's `tmp/` directory for an upload.
fn temp_path(zone: &Zone, p: PlayerId) -> Option<PathBuf> {
    let dir = zone_root(zone)?.join("tmp");
    std::fs::create_dir_all(&dir).ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Some(dir.join(format!("uploaded-{}-{nanos}", p.0)))
}

fn on_upload(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    let wanted = ft(zone, p).is_some_and(|f| f.pending.is_some());
    let allowed = zone
        .iface::<dyn super::capman::Capabilities>()
        .is_none_or(|c| c.has_capability(zone, p, CAP_UPLOAD));
    if !allowed {
        tracing::info!(target: "filetrans", pid = p.0, "denied file upload");
        finish(zone, p, None);
        return;
    }
    let Ok(file) = c2s::UploadFile::decode(pkt) else {
        tracing::warn!(target: "filetrans", pid = p.0, len = pkt.len(), "bad upload packet");
        finish(zone, p, None);
        return;
    };
    if !wanted {
        tracing::info!(target: "filetrans", pid = p.0, "unasked-for upload, dropped");
        return;
    }
    if file.data.len() > MAX_FILE {
        tracing::warn!(target: "filetrans", pid = p.0, bytes = file.data.len(), "upload too big");
        finish(zone, p, None);
        return;
    }
    let path = temp_path(zone, p);
    let written = match &path {
        Some(path) => match std::fs::write(path, &file.data) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(target: "filetrans", file = %path.display(), "can't write upload: {e}");
                false
            }
        },
        None => {
            tracing::warn!(target: "filetrans", pid = p.0, "nowhere to put an upload");
            false
        }
    };
    if written {
        tracing::info!(target: "filetrans", pid = p.0, bytes = file.data.len(), "completed upload");
    }
    finish(zone, p, path.filter(|_| written));
}

/// How many files `?putzip` will take out of one archive.
const MAX_ZIP_ENTRIES: usize = 4096;

/// Reads a zip archive's central directory and writes its files into `dir`.
///
/// ASSS shells out to `unzip -qq -a -o -j`; this does the same job in
/// process, and keeps the `-j`: the paths inside the archive are ignored and
/// every file lands in `dir` under its own name, so an archive can't write
/// outside the directory it was aimed at. Stored and deflated entries are
/// taken, anything else is skipped, and the whole archive may not produce
/// more than [`MAX_FILE`] bytes. Returns how many files were written.
///
/// The archive comes from a client, so every field is bounds-checked and
/// nothing here can panic on a malformed one.
pub fn unzip_into(data: &[u8], dir: &Path) -> Result<usize, &'static str> {
    // the end-of-central-directory record is at the end, after a comment of
    // up to 64k
    let tail = data.len().saturating_sub(22 + 0xFFFF);
    let eocd = (tail..data.len().saturating_sub(21))
        .rev()
        .find(|&i| data[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .ok_or("not a zip file")?;
    let le16 = |at: usize| -> Option<usize> {
        let b = data.get(at..at + 2)?;
        Some(usize::from(u16::from_le_bytes([b[0], b[1]])))
    };
    let le32 = |at: usize| -> Option<usize> {
        let b = data.get(at..at + 4)?;
        usize::try_from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok()
    };
    let entries = le16(eocd + 10).ok_or("truncated zip")?;
    let mut at = le32(eocd + 16).ok_or("truncated zip")?;
    std::fs::create_dir_all(dir).map_err(|_| "can't create the directory")?;
    let (mut written, mut total) = (0usize, 0usize);
    for _ in 0..entries.min(MAX_ZIP_ENTRIES) {
        if data
            .get(at..at + 46)
            .is_none_or(|h| h[..4] != [0x50, 0x4b, 0x01, 0x02])
        {
            break;
        }
        let method = le16(at + 10).ok_or("truncated zip")?;
        let comp_size = le32(at + 20).ok_or("truncated zip")?;
        let name_len = le16(at + 28).ok_or("truncated zip")?;
        let extra_len = le16(at + 30).ok_or("truncated zip")?;
        let comment_len = le16(at + 32).ok_or("truncated zip")?;
        let local = le32(at + 42).ok_or("truncated zip")?;
        let name = data
            .get(at + 46..at + 46 + name_len)
            .map(crate::config::latin1)
            .unwrap_or_default();
        at = at + 46 + name_len + extra_len + comment_len;
        // `unzip -j`: the archive's own directories are ignored
        let name = client_basename(&name);
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        // the local header repeats the name and extra fields, with its own
        // lengths; the data starts after them
        let Some(lh) = data.get(local..local + 30) else {
            continue;
        };
        if lh[..4] != [0x50, 0x4b, 0x03, 0x04] {
            continue;
        }
        let lname = le16(local + 26).ok_or("truncated zip")?;
        let lextra = le16(local + 28).ok_or("truncated zip")?;
        let start = local + 30 + lname + lextra;
        let Some(body) = data.get(start..start.saturating_add(comp_size)) else {
            continue;
        };
        let out = match method {
            0 => body.to_vec(),
            8 => {
                let mut buf = Vec::new();
                let mut r = flate2::read::DeflateDecoder::new(body).take(MAX_FILE as u64);
                if r.read_to_end(&mut buf).is_err() {
                    continue;
                }
                buf
            }
            // stored and deflated are what zip files of zone content use
            _ => continue,
        };
        total = total.saturating_add(out.len());
        if total > MAX_FILE {
            return Err("the archive holds too much");
        }
        if std::fs::write(dir.join(&name), &out).is_ok() {
            written += 1;
        }
    }
    Ok(written)
}

/// The `filetrans` module.
pub struct FileTransModule;

impl Module for FileTransModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::UPLOAD_FILE, on_upload);
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if ev.action == Action::Disconnect {
                finish(z, ev.player, None);
            }
        });
        Ok(())
    }
}

// ---- sendfile: offering a file to another player --------------------------

/// A file one player has offered another, waiting for `?acceptfile`.
#[derive(Debug, Clone)]
struct Offer {
    from: PlayerId,
    to: PlayerId,
    /// Path on the offering player's own machine.
    client_path: String,
}

#[derive(Default)]
struct Offers(Vec<Offer>);

fn offers(zone: &mut Zone) -> &mut Vec<Offer> {
    &mut zone.res.get_or_default::<Offers>().0
}

/// Drops every offer a player is part of; true if there was one.
fn cancel_offers(zone: &mut Zone, p: PlayerId) -> bool {
    let list = offers(zone);
    let before = list.len();
    list.retain(|o| o.from != p && o.to != p);
    before != offers(zone).len()
}

/// The last component of a path the client gave us, by either separator.
pub fn client_basename(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = if name.is_empty() { "file" } else { name };
    name.chars().take(16).collect()
}

fn c_sendfile(zone: &mut Zone, inv: &crate::commands::Invocation<'_>) {
    let crate::commands::Target::Player(to) = inv.target else {
        return;
    };
    let path = inv.params.trim().to_owned();
    if path.is_empty() {
        return;
    }
    let from = inv.sender;
    let standard = |z: &Zone, p: PlayerId| {
        z.players
            .get(p)
            .is_some_and(|x| matches!(x.kind, ClientKind::Vie | ClientKind::Continuum))
    };
    if !standard(zone, from) {
        message(zone, from, "You must be in a game client to offer files");
        return;
    }
    if !standard(zone, to) {
        message(
            zone,
            from,
            "You can only offer files to players in game clients",
        );
        return;
    }
    if to == from {
        message(zone, from, "You cannot send files to yourself");
        return;
    }
    if offers(zone).iter().any(|o| o.from == from) {
        message(
            zone,
            from,
            "You are currently sending a file. Use ?cancelfile to cancel the current file transfer.",
        );
        return;
    }
    if offers(zone).iter().any(|o| o.to == to) {
        message(zone, from, "That player is currently receiving a file");
        return;
    }
    let spec = |z: &Zone, p: PlayerId| z.players.get(p).is_some_and(|x| x.ship.is_spec());
    if !spec(zone, from) {
        message(zone, from, "You must be in spectator mode to offer files");
        return;
    }
    if !spec(zone, to) {
        message(
            zone,
            from,
            "You must offer files to another player in spectator mode",
        );
        return;
    }
    let (from_name, to_name) = (
        zone.players
            .get(from)
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        zone.players
            .get(to)
            .map(|p| p.name.clone())
            .unwrap_or_default(),
    );
    offers(zone).push(Offer {
        from,
        to,
        client_path: path.clone(),
    });
    message(
        zone,
        from,
        &format!("Waiting for {to_name} to accept your file."),
    );
    message(
        zone,
        to,
        &format!(
            "{from_name} wants to send you the file \"{path}\". To accept it, type ?acceptfile."
        ),
    );
}

fn c_cancelfile(zone: &mut Zone, inv: &crate::commands::Invocation<'_>) {
    if cancel_offers(zone, inv.sender) {
        message(zone, inv.sender, "Your file offers have been cancelled.");
    }
}

fn c_acceptfile(zone: &mut Zone, inv: &crate::commands::Invocation<'_>) {
    let me = inv.sender;
    let Some(i) = offers(zone).iter().position(|o| o.to == me) else {
        message(zone, me, "Nobody has offered any files to you.");
        return;
    };
    let offer = offers(zone).remove(i);
    let my_name = zone
        .players
        .get(me)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    message(
        zone,
        offer.from,
        &format!("{my_name} is accepting your file."),
    );
    message(zone, me, "File accepted. Transferring...");
    let to = offer.to;
    let name = client_basename(&offer.client_path);
    let asked = request_file(
        zone,
        offer.from,
        &offer.client_path,
        Box::new(move |z: &mut Zone, from: PlayerId, file: Option<PathBuf>| {
            let Some(file) = file else {
                tracing::warn!(target: "sendfile", "upload failed");
                return;
            };
            let ready = z
                .players
                .get(to)
                .is_some_and(|p| p.status == PlayerStatus::Playing);
            if !ready || !send_file(z, to, &file, &name, true) {
                let _ = std::fs::remove_file(&file);
                tracing::warn!(target: "sendfile", "can't pass the file on to the recipient");
                return;
            }
            message(z, from, "File sent.");
        }),
    );
    if !asked {
        message(zone, me, "That player can't send a file just now.");
    }
}

/// The `sendfile` module: `?sendfile`, `?acceptfile`, `?cancelfile`.
pub struct SendFileModule;

impl Module for SendFileModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.add_command(
            me,
            "sendfile",
            None,
            Some(
                "Module: sendfile\nTargets: player\nArgs: <path on your machine>\nOffers someone a file from your own client's directory. Only one file can be offered at once, and both of you must be spectating.\n",
            ),
            c_sendfile,
        );
        zone.add_command(
            me,
            "acceptfile",
            None,
            Some("Module: sendfile\nTargets: none\nArgs: none\nAccepts a file someone has offered you.\n"),
            c_acceptfile,
        );
        zone.add_command(
            me,
            "cancelfile",
            None,
            Some("Module: sendfile\nTargets: none\nArgs: none\nWithdraws the file you offered.\n"),
            c_cancelfile,
        );
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            if matches!(ev.action, Action::Connect | Action::Disconnect) {
                cancel_offers(z, ev.player);
            }
        });
        Ok(())
    }
}
