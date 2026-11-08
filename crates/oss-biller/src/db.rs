//! SQLite storage: accounts, scores and squads.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// Storage errors.
pub type Result<T> = rusqlite::Result<T>;

/// One account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// User id.
    pub id: u32,
    /// Name as first registered.
    pub name: String,
    /// When the account was created (Unix seconds).
    pub created: i64,
    /// Total seconds played.
    pub seconds: u32,
    /// Banner (96 bytes, or empty).
    pub banner: Vec<u8>,
    /// Squad, or empty.
    pub squad: String,
    /// `?chat=` channel list, comma separated.
    pub chats: String,
    /// Banned.
    pub banned: bool,
}

/// A squad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Squad {
    /// Name as created.
    pub name: String,
    /// Owner's user id.
    pub owner: u32,
}

/// Scores for one score realm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Score {
    /// Kills.
    pub kills: u16,
    /// Deaths.
    pub deaths: u16,
    /// Flags.
    pub flags: u16,
    /// Kill points.
    pub points: u32,
    /// Flag points.
    pub flag_points: u32,
}

/// The database.
pub struct Store {
    db: Connection,
    entropy: oss_net::opensec::Entropy,
}

const ROUNDS: usize = 4096;

fn hash(salt: &[u8], password: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password.as_bytes());
    let mut d = h.finalize();
    for _ in 1..ROUNDS {
        let mut h = Sha256::new();
        h.update(d);
        h.update(salt);
        d = h.finalize();
    }
    d.to_vec()
}

fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const USER_COLS: &str = "id, name, created, seconds, banner, squad, chats, banned";

fn user_row(r: &rusqlite::Row<'_>) -> Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        created: r.get(2)?,
        seconds: r.get(3)?,
        banner: r.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default(),
        squad: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        chats: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        banned: r.get::<_, i64>(7)? != 0,
    })
}

impl Store {
    /// Opens (creating if needed) a database file.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(db)
    }

    /// A database in memory (for tests).
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                salt BLOB NOT NULL,
                hash BLOB NOT NULL,
                created INTEGER NOT NULL,
                seconds INTEGER NOT NULL DEFAULT 0,
                banner BLOB,
                squad TEXT,
                chats TEXT,
                banned INTEGER NOT NULL DEFAULT 0,
                demographics BLOB
             );
             CREATE TABLE IF NOT EXISTS scores (
                user_id INTEGER NOT NULL,
                score_id INTEGER NOT NULL,
                kills INTEGER NOT NULL,
                deaths INTEGER NOT NULL,
                flags INTEGER NOT NULL,
                points INTEGER NOT NULL,
                flag_points INTEGER NOT NULL,
                PRIMARY KEY (user_id, score_id)
             );
             CREATE TABLE IF NOT EXISTS squads (
                name TEXT PRIMARY KEY COLLATE NOCASE,
                salt BLOB NOT NULL,
                hash BLOB NOT NULL,
                owner INTEGER NOT NULL,
                created INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            db,
            entropy: oss_net::opensec::os_entropy(),
        })
    }

    fn salt(&self) -> [u8; 16] {
        let mut s = [0u8; 16];
        (self.entropy)(&mut s);
        s
    }

    /// Looks an account up by name (case-insensitive).
    pub fn user(&self, name: &str) -> Result<Option<User>> {
        self.db
            .query_row(
                &format!("SELECT {USER_COLS} FROM users WHERE name = ?1"),
                params![name],
                user_row,
            )
            .optional()
    }

    /// Looks an account up by id.
    pub fn user_by_id(&self, id: u32) -> Result<Option<User>> {
        self.db
            .query_row(
                &format!("SELECT {USER_COLS} FROM users WHERE id = ?1"),
                params![id],
                user_row,
            )
            .optional()
    }

    /// Creates an account.
    pub fn create_user(&self, name: &str, password: &str, now: i64) -> Result<User> {
        let salt = self.salt();
        self.db.execute(
            "INSERT INTO users (name, salt, hash, created) VALUES (?1, ?2, ?3, ?4)",
            params![name, &salt[..], hash(&salt, password), now],
        )?;
        self.user(name)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    /// True if `password` is the account's password.
    pub fn check_password(&self, id: u32, password: &str) -> Result<bool> {
        let (salt, stored): (Vec<u8>, Vec<u8>) = self.db.query_row(
            "SELECT salt, hash FROM users WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(eq_ct(&hash(&salt, password), &stored))
    }

    /// Changes a password.
    pub fn set_password(&self, id: u32, password: &str) -> Result<()> {
        let salt = self.salt();
        self.db.execute(
            "UPDATE users SET salt = ?2, hash = ?3 WHERE id = ?1",
            params![id, &salt[..], hash(&salt, password)],
        )?;
        Ok(())
    }

    /// Adds play time.
    pub fn add_seconds(&self, id: u32, secs: u32) -> Result<()> {
        self.db.execute(
            "UPDATE users SET seconds = seconds + ?2 WHERE id = ?1",
            params![id, secs],
        )?;
        Ok(())
    }

    /// Stores a banner.
    pub fn set_banner(&self, id: u32, banner: &[u8]) -> Result<()> {
        self.db.execute(
            "UPDATE users SET banner = ?2 WHERE id = ?1",
            params![id, banner],
        )?;
        Ok(())
    }

    /// Stores a `?chat=` list.
    pub fn set_chats(&self, id: u32, chats: &str) -> Result<()> {
        self.db.execute(
            "UPDATE users SET chats = ?2 WHERE id = ?1",
            params![id, chats],
        )?;
        Ok(())
    }

    /// Stores registration data.
    pub fn set_demographics(&self, id: u32, data: &[u8]) -> Result<()> {
        self.db.execute(
            "UPDATE users SET demographics = ?2 WHERE id = ?1",
            params![id, data],
        )?;
        Ok(())
    }

    /// Bans or unbans an account.
    pub fn set_banned(&self, name: &str, banned: bool) -> Result<bool> {
        Ok(self.db.execute(
            "UPDATE users SET banned = ?2 WHERE name = ?1",
            params![name, banned],
        )? > 0)
    }

    /// A user's score in a realm.
    pub fn score(&self, id: u32, score_id: u32) -> Result<Option<Score>> {
        self.db
            .query_row(
                "SELECT kills, deaths, flags, points, flag_points FROM scores
                 WHERE user_id = ?1 AND score_id = ?2",
                params![id, score_id],
                |r| {
                    Ok(Score {
                        kills: r.get(0)?,
                        deaths: r.get(1)?,
                        flags: r.get(2)?,
                        points: r.get(3)?,
                        flag_points: r.get(4)?,
                    })
                },
            )
            .optional()
    }

    /// Stores a user's score in a realm.
    pub fn set_score(&self, id: u32, score_id: u32, s: &Score) -> Result<()> {
        self.db.execute(
            "INSERT INTO scores (user_id, score_id, kills, deaths, flags, points, flag_points)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (user_id, score_id) DO UPDATE SET kills = ?3, deaths = ?4,
                flags = ?5, points = ?6, flag_points = ?7",
            params![
                id,
                score_id,
                s.kills,
                s.deaths,
                s.flags,
                s.points,
                s.flag_points
            ],
        )?;
        Ok(())
    }

    /// Clears every score in a realm.
    pub fn reset_scores(&self, score_id: u32) -> Result<()> {
        self.db
            .execute("DELETE FROM scores WHERE score_id = ?1", params![score_id])?;
        Ok(())
    }

    // ---- squads -------------------------------------------------------------

    /// Looks a squad up.
    pub fn squad(&self, name: &str) -> Result<Option<Squad>> {
        self.db
            .query_row(
                "SELECT name, owner FROM squads WHERE name = ?1",
                params![name],
                |r| {
                    Ok(Squad {
                        name: r.get(0)?,
                        owner: r.get(1)?,
                    })
                },
            )
            .optional()
    }

    /// Creates a squad owned by `owner`, who joins it.
    pub fn create_squad(&self, name: &str, password: &str, owner: u32, now: i64) -> Result<()> {
        let salt = self.salt();
        self.db.execute(
            "INSERT INTO squads (name, salt, hash, owner, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, &salt[..], hash(&salt, password), owner, now],
        )?;
        self.set_squad(owner, name)
    }

    /// True if `password` is the squad's password.
    pub fn check_squad_password(&self, name: &str, password: &str) -> Result<bool> {
        let (salt, stored): (Vec<u8>, Vec<u8>) = self.db.query_row(
            "SELECT salt, hash FROM squads WHERE name = ?1",
            params![name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(eq_ct(&hash(&salt, password), &stored))
    }

    /// Changes a squad's password.
    pub fn set_squad_password(&self, name: &str, password: &str) -> Result<()> {
        let salt = self.salt();
        self.db.execute(
            "UPDATE squads SET salt = ?2, hash = ?3 WHERE name = ?1",
            params![name, &salt[..], hash(&salt, password)],
        )?;
        Ok(())
    }

    /// Gives a squad to another member.
    pub fn set_squad_owner(&self, name: &str, owner: u32) -> Result<()> {
        self.db.execute(
            "UPDATE squads SET owner = ?2 WHERE name = ?1",
            params![name, owner],
        )?;
        Ok(())
    }

    /// Sets (or with `""` clears) a user's squad.
    pub fn set_squad(&self, id: u32, squad: &str) -> Result<()> {
        self.db.execute(
            "UPDATE users SET squad = ?2 WHERE id = ?1",
            params![id, squad],
        )?;
        Ok(())
    }

    /// Names of a squad's members.
    pub fn squad_members(&self, name: &str) -> Result<Vec<String>> {
        let mut st = self
            .db
            .prepare("SELECT name FROM users WHERE squad = ?1 COLLATE NOCASE ORDER BY name")?;
        let rows = st.query_map(params![name], |r| r.get(0))?;
        rows.collect()
    }

    /// Deletes a squad; its members are left squadless.
    pub fn dissolve_squad(&self, name: &str) -> Result<()> {
        self.db.execute(
            "UPDATE users SET squad = '' WHERE squad = ?1 COLLATE NOCASE",
            params![name],
        )?;
        self.db
            .execute("DELETE FROM squads WHERE name = ?1", params![name])?;
        Ok(())
    }
}

/// Unix seconds as (year, month, day, hour, minute, second), UTC.
pub fn civil(t: i64) -> [u16; 6] {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    // Howard Hinnant's days-to-civil
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    [
        u16::try_from(y).unwrap_or(0),
        m as u16,
        d as u16,
        (secs / 3600) as u16,
        (secs % 3600 / 60) as u16,
        (secs % 60) as u16,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_and_squads() {
        let s = Store::in_memory().expect("db");
        let u = s.create_user("Alice", "pw", 1_060_012_551).expect("create");
        assert_eq!(s.user("alice").expect("q").map(|x| x.id), Some(u.id));
        assert!(s.check_password(u.id, "pw").expect("pw"));
        assert!(!s.check_password(u.id, "PW").expect("pw"));
        assert!(s.create_user("ALICE", "x", 0).is_err());
        s.create_squad("Hawks", "sq", u.id, 0).expect("squad");
        assert_eq!(
            s.user_by_id(u.id).expect("q").map(|x| x.squad),
            Some("Hawks".into())
        );
        assert!(s.check_squad_password("hawks", "sq").expect("pw"));
        assert_eq!(
            s.squad_members("HAWKS").expect("m"),
            vec!["Alice".to_owned()]
        );
        s.dissolve_squad("hawks").expect("dissolve");
        assert!(s.squad("hawks").expect("q").is_none());
        let sc = Score {
            kills: 1,
            points: 5,
            ..Score::default()
        };
        s.set_score(u.id, 3, &sc).expect("score");
        assert_eq!(s.score(u.id, 3).expect("q"), Some(sc));
        assert_eq!(s.score(u.id, 4).expect("q"), None);
    }

    #[test]
    fn dates() {
        assert_eq!(civil(1_060_012_551), [2003, 8, 4, 15, 55, 51]);
        assert_eq!(civil(0), [1970, 1, 1, 0, 0, 0]);
        assert_eq!(civil(951_782_400), [2000, 2, 29, 0, 0, 0]);
    }
}
