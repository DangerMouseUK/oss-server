//! `turf_reward` and `points_turf_reward`: weighted turf scoring (ASSS
//! `turf/turf_reward.c` and `turf/points_turf_reward.c`). Attach both to a
//! turf arena, alongside `fg_turf`.
//!
//! Every `TurfReward:TimerInterval` ticks the arena *dings*: each team's
//! flags are counted, turned into points by a scoring algorithm, and paid
//! out as flag points. What makes it more than [`super::periodic`] is that
//! flags are **weighted**: a flag is worth `Weight<n>` where `n` is how long
//! its team has held it (dings by default, minutes with `WeightCalc`), so
//! holding ground is worth more than taking it.
//!
//! **Recovery.** When a team loses a flag, its weight is remembered for a
//! while (`RecoverDings`, `RecoverTime`, `RecoveryCutoff`, `RecoverMax`): if
//! they tag it back within that window the flag returns at its old weight
//! and ding count rather than starting again. Several teams can be waiting
//! to recover the same flag at once, each at their own weight.
//!
//! **Scoring is pluggable**, as in ASSS: `turf_reward` collects the state
//! and a [`TurfRewardPoints`] decides what it is worth. `points_turf_reward`
//! is the built-in one and implements every style ASSS ships:
//!
//! - `TR_STYLE_DISABLED` — no rewards.
//! - `TR_STYLE_PERIODIC` — `RewardModifier` points per flag, or, when the
//!   modifier is negative, that many per flag per player in the arena; zero
//!   means dings happen but pay nothing.
//! - `TR_STYLE_STANDARD` — the weighted algorithm: a jackpot of
//!   `RewardModifier * players` is split between teams by their share of
//!   weights *per player*, so a bigger team earns less each.
//! - `TR_STYLE_STD_BTY` — the same, with the jackpot being the bounty
//!   exchanged in kills since the last ding.
//! - `TR_STYLE_WEIGHTS` — a team's points are its weights.
//! - `TR_STYLE_FIXED_PTS` — 1000, then 500, then nothing, by placement.
//!
//! **Not ported, deliberately.** ASSS's `turf_stats` (`?turfstats` and the
//! history it keeps) is a separate module and is not here. Neither is the
//! persistent flag ownership that `turf_reward` saves through `persist`,
//! which is the same gap as `Flag:PersistentTurfOwners` in
//! [`super::fg_turf`]: after a restart every flag starts unowned. In
//! `TR_STYLE_FIXED_PTS`, ASSS sorts teams with a comparator that returns a
//! bool where an ordering is wanted and says in a comment that it doesn't
//! know which way the sort runs; this sorts by the standard algorithm's
//! points, highest first, so the team the weighted scoring rates best takes
//! 1000.

use std::collections::HashMap;
use std::sync::Arc;

use crate::arena::ArenaId;
use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, PlayerStatus};
use crate::registry::{Event, Owner, Scope};
use crate::time::{TICKS_PER_SECOND, Ticks};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind};
use super::chat::{Chat, message};
use super::fg_turf::TurfTag;
use super::game::Kill;
use super::stats::{Stats, stat};

/// ASSS's `SOUND_DING`.
const SOUND_DING: u8 = 103;

/// The safe-zone status bit.
const SAFE_ZONE: u8 = 0x20;

/// How weights are counted (`TurfReward:WeightCalc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WeightCalc {
    /// One weight step per ding of ownership.
    #[default]
    Dings,
    /// One weight step per minute of ownership.
    Time,
}

/// When a team stops being able to recover a flag
/// (`TurfReward:RecoveryCutoff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecoveryCutoff {
    /// After `RecoverDings` dings.
    #[default]
    Dings,
    /// After `RecoverTime` seconds.
    Time,
    /// Whichever comes first.
    DingsAndTime,
}

/// The scoring algorithm (`TurfReward:RewardStyle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RewardStyle {
    /// No rewards.
    #[default]
    Disabled,
    /// Points per flag.
    Periodic,
    /// The weighted per-capita algorithm.
    Standard,
    /// Standard, with the jackpot taken from bounty exchanged.
    StdBty,
    /// Points equal weights.
    Weights,
    /// A fixed amount by placement.
    FixedPts,
    /// A style the module does not recognise. ASSS reaches its
    /// `CALC_REWARD_FAILED` macro here, which -- despite the name -- reports
    /// success with nothing awarded, so the ding timer keeps running. That
    /// is a different state from [`RewardStyle::Disabled`], which leaves the
    /// failure `preCalc` wrote and halts further rewards.
    Other,
}

impl RewardStyle {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_uppercase().as_str() {
            "TR_STYLE_DISABLED" | "DISABLED" => Self::Disabled,
            "TR_STYLE_PERIODIC" | "PERIODIC" => Self::Periodic,
            "TR_STYLE_STANDARD" | "STANDARD" => Self::Standard,
            "TR_STYLE_STD_BTY" | "STD_BTY" => Self::StdBty,
            "TR_STYLE_WEIGHTS" | "WEIGHTS" => Self::Weights,
            "TR_STYLE_FIXED_PTS" | "FIXED_PTS" => Self::FixedPts,
            _ => return None,
            // a name nobody recognises is `Other`, not `Disabled`; see the
            // variant's own note. `load` maps a missing key to the default.
        })
    }
}

/// `[TurfReward]` for an arena.
#[derive(Debug, Clone)]
pub struct TurfSettings {
    /// Which scoring algorithm runs.
    pub reward_style: RewardStyle,
    /// Players a team needs before it can be paid.
    pub min_players_on_team: i32,
    /// Players the arena needs before anyone is paid.
    pub min_players_in_arena: i32,
    /// Teams the arena needs before anyone is paid.
    pub min_teams: i32,
    /// Flags a team needs.
    pub min_flags: i32,
    /// Share of the map's flags a team needs, in percent.
    pub min_percent_flags: f64,
    /// Weights a team needs.
    pub min_weights: i64,
    /// Share of the arena's weights a team needs, in percent.
    pub min_percent_weights: f64,
    /// Share of the jackpot a team needs, in percent.
    pub min_percent: f64,
    /// What the jackpot is made of; the meaning depends on the style.
    pub reward_modifier: i32,
    /// Most points one player can be given in one ding.
    pub max_points: i32,
    /// Ticks before the first ding.
    pub timer_initial: u64,
    /// Ticks between dings.
    pub timer_interval: u64,
    /// Spectators are paid too.
    pub spec_receive_points: bool,
    /// Players in safe zones are paid too.
    pub safe_receive_points: bool,
    /// How a recovery window ends.
    pub recovery_cutoff: RecoveryCutoff,
    /// Dings a team has to tag a flag back.
    pub recover_dings: u32,
    /// Seconds a team has to tag a flag back.
    pub recover_time: u64,
    /// Times one flag may be recovered (-1 = no limit).
    pub recover_max: i32,
    /// How ownership turns into a weight step.
    pub weight_calc: WeightCalc,
    /// `Weight0`, `Weight1`, … The last one is used from then on.
    pub weights: Vec<i32>,
}

impl Default for TurfSettings {
    fn default() -> Self {
        Self {
            reward_style: RewardStyle::Disabled,
            min_players_on_team: 3,
            min_players_in_arena: 6,
            min_teams: 2,
            min_flags: 1,
            min_percent_flags: 0.0,
            min_weights: 1,
            min_percent_weights: 0.0,
            min_percent: 0.0,
            reward_modifier: 200,
            max_points: 10000,
            timer_initial: 6000,
            timer_interval: 6000,
            spec_receive_points: false,
            safe_receive_points: false,
            recovery_cutoff: RecoveryCutoff::Dings,
            recover_dings: 1,
            recover_time: 300,
            recover_max: -1,
            weight_calc: WeightCalc::Dings,
            weights: vec![1],
        }
    }
}

impl TurfSettings {
    /// Reads `[TurfReward]`, with ASSS's floors on the values that would
    /// otherwise divide by zero or ding faster than a server can answer.
    pub fn load(cfg: &crate::config::ConfigFile) -> Self {
        let i = |k: &str, d: i32| cfg.get_int("TurfReward", k, d);
        let mut s = Self {
            reward_style: cfg
                .get("TurfReward", "RewardStyle")
                .and_then(RewardStyle::parse)
                .unwrap_or(RewardStyle::Disabled),
            min_players_on_team: i("MinPlayersTeam", 3),
            min_players_in_arena: i("MinPlayersArena", 6),
            min_teams: i("MinTeams", 2),
            min_flags: i("MinFlags", 1),
            min_percent_flags: f64::from(i("MinFlagsPercent", 0)) / 1000.0,
            min_weights: i64::from(i("MinWeights", 1)),
            min_percent_weights: f64::from(i("MinWeightsPercent", 0)) / 1000.0,
            min_percent: f64::from(i("MinPercent", 0)) / 1000.0,
            reward_modifier: i("RewardModifier", 200),
            max_points: i("MaxPoints", 10000),
            timer_initial: u64::try_from(i("TimerInitial", 6000)).unwrap_or(6000),
            timer_interval: u64::try_from(i("TimerInterval", 6000)).unwrap_or(6000),
            spec_receive_points: i("SpecRecievePoints", 0) != 0,
            safe_receive_points: i("SafeRecievePoints", 0) != 0,
            recovery_cutoff: match cfg
                .get("TurfReward", "RecoveryCutoff")
                .unwrap_or("")
                .trim()
                .to_ascii_uppercase()
                .as_str()
            {
                "TR_RECOVERY_TIME" | "TIME" => RecoveryCutoff::Time,
                "TR_RECOVERY_DINGS_AND_TIME" | "DINGS_AND_TIME" => RecoveryCutoff::DingsAndTime,
                _ => RecoveryCutoff::Dings,
            },
            recover_dings: u32::try_from(i("RecoverDings", 1)).unwrap_or(1),
            recover_time: u64::try_from(i("RecoverTime", 300)).unwrap_or(300),
            recover_max: i("RecoverMax", -1),
            weight_calc: match cfg
                .get("TurfReward", "WeightCalc")
                .unwrap_or("")
                .trim()
                .to_ascii_uppercase()
                .as_str()
            {
                "TR_WEIGHT_TIME" | "TIME" => WeightCalc::Time,
                _ => WeightCalc::Dings,
            },
            weights: Vec::new(),
        };
        let set = i("SetWeights", 0).clamp(0, 100);
        if set < 1 {
            s.weights = vec![1];
        } else {
            // an unset weight keeps the previous one, so a short list
            // neither rises nor falls
            let mut last = 1;
            for n in 0..set {
                last = i(&format!("Weight{n}"), last);
                s.weights.push(last);
            }
        }
        s.min_players_on_team = s.min_players_on_team.max(1);
        s.min_players_in_arena = s.min_players_in_arena.max(1);
        s.min_teams = s.min_teams.max(1);
        s.min_flags = s.min_flags.max(1);
        s.min_weights = s.min_weights.max(1);
        // 15 seconds, so a zone can't ding faster than the server can pay
        s.timer_initial = s.timer_initial.max(1500);
        s.timer_interval = s.timer_interval.max(1500);
        s
    }
}

/// A team that used to own a flag and may yet tag it back.
#[derive(Debug, Clone)]
pub struct Previous {
    /// Dings since they lost it.
    pub last_owned: u32,
    /// Who had tagged it for them.
    pub tagger: String,
    /// Their frequency.
    pub freq: i16,
    /// The ding count it had.
    pub dings: u32,
    /// The weight it had.
    pub weight: i32,
    /// How many times it had been recovered.
    pub recovered: u32,
    /// When it was first tagged.
    pub tag_tc: Ticks,
    /// When they lost it.
    pub lost_tc: Ticks,
}

/// One turf flag, as the reward system sees it.
#[derive(Debug, Clone, Default)]
pub struct TurfFlag {
    /// Owning frequency, or -1.
    pub freq: i16,
    /// Who tagged it.
    pub tagger: String,
    /// Dings it has been held for.
    pub dings: u32,
    /// What it is worth.
    pub weight: i32,
    /// Times it has been recovered.
    pub recovered: u32,
    /// When it was first tagged.
    pub tag_tc: Ticks,
    /// When it was last tagged.
    pub last_tc: Ticks,
    /// Teams that may still tag it back.
    pub old: Vec<Previous>,
}

/// What a player has done since the last ding.
#[derive(Debug, Clone, Default)]
pub struct TurfPlayerStats {
    /// Flags tagged.
    pub tags: u32,
    /// Flags taken from another team.
    pub steals: u32,
    /// Flags tagged back inside the recovery window.
    pub recoveries: u32,
    /// Flags of theirs another team took.
    pub lost: u32,
    /// Kills made.
    pub kills: u32,
    /// Bounty taken in those kills.
    pub bounty_taken: u32,
    /// Deaths.
    pub deaths: u32,
    /// Points this ding, for a per-player algorithm.
    pub points: i32,
}

/// A team at the moment of a ding. Rebuilt every time.
#[derive(Debug, Clone, Default)]
pub struct TurfTeam {
    /// The frequency.
    pub freq: i16,
    /// Flags it owns.
    pub num_flags: u32,
    /// Share of the map's flags, in percent.
    pub percent_flags: f64,
    /// Sum of its flags' weights.
    pub num_weights: i64,
    /// Share of the arena's weights, in percent.
    pub percent_weights: f64,
    /// Flags per player, truncated (see [`TurfData::sum_per_capita_flags`]).
    pub per_capita_flags: f64,
    /// Weights per player, truncated.
    pub per_capita_weights: f64,
    /// Players who may be paid.
    pub num_players: u32,
    /// Share of the jackpot, in percent.
    pub percent: f64,
    /// Points for each of its players.
    pub num_points: i32,
    /// Players being paid.
    pub players_pts: Vec<PlayerId>,
    /// Players present but not being paid (spectating, in a safe zone).
    pub players_no_pts: Vec<PlayerId>,
    /// Whether it met every minimum.
    pub valid: bool,
}

/// An arena's turf reward state.
#[derive(Debug, Clone, Default)]
pub struct TurfData {
    /// The arena's settings.
    pub settings: TurfSettings,
    /// Every turf flag on the map.
    pub flags: Vec<TurfFlag>,
    /// When the last ding was.
    pub ding_time: Ticks,
    /// When the next one is due.
    pub next_ding: Ticks,
    /// Set once scoring reported [`CalcStatus::Fail`]: the arena stops
    /// dinging until the timer is reset.
    pub ding_stopped: bool,
    /// Players in the arena.
    pub num_players: i32,
    /// Players on teams that met every minimum.
    pub num_valid_players: i32,
    /// Teams present.
    pub num_teams: i32,
    /// Teams that met every minimum.
    pub num_valid_teams: i32,
    /// Weights owned in the arena.
    pub num_weights: i64,
    /// Sum of every valid team's flags per player. ASSS keeps this beside
    /// the weights figure and reports it, but no scoring style it ships
    /// reads it; it is here because the module's state is observable.
    pub sum_per_capita_flags: f64,
    /// Sum of every valid team's weights per player.
    pub sum_per_capita_weights: f64,
    /// The jackpot this ding.
    pub num_points: i64,
    /// Flags tagged since the last ding.
    pub tags: u32,
    /// Flags stolen since the last ding.
    pub steals: u32,
    /// Flags recovered since the last ding.
    pub recoveries: u32,
    /// Flags lost since the last ding.
    pub lost: u32,
    /// Kills since the last ding.
    pub kills: u32,
    /// Bounty exchanged in those kills.
    pub bounty_exchanged: i64,
    /// The teams, rebuilt each ding.
    pub teams: Vec<TurfTeam>,
    /// Per-player counters, by name.
    pub players: HashMap<String, TurfPlayerStats>,
}

/// Who gets paid for what a [`TurfRewardPoints`] worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Award {
    /// Each player gets their own `points`.
    Player,
    /// Each player gets their team's `num_points`.
    Team,
    /// Both, added together.
    Both,
    /// Nobody is paid.
    None,
}

/// Whether the ding happened at all (ASSS `TurfState.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalcStatus {
    /// The scoring module ran. Points may still be zero.
    Success,
    /// The ding did not happen; `doReward` stops rewarding this arena.
    Fail,
}

/// What a scoring algorithm decided (ASSS `TurfState`).
#[derive(Debug, Clone, Copy)]
pub struct CalcState {
    /// Whether the ding counted.
    pub status: CalcStatus,
    /// Who to pay.
    pub award: Award,
    /// Whether flags age by a ding afterwards.
    pub update: bool,
}

impl CalcState {
    /// The ding did not happen: nothing is paid, the flags don't age, no
    /// post-reward callback runs, and the arena stops dinging. Only
    /// [`RewardStyle::Disabled`] reaches this among the styles ASSS ships.
    pub fn failed() -> Self {
        Self {
            status: CalcStatus::Fail,
            award: Award::None,
            update: false,
        }
    }

    /// The ding happened but pays nothing and ages no flags (ASSS's
    /// confusingly named `CALC_REWARD_FAILED`).
    pub fn no_award() -> Self {
        Self {
            status: CalcStatus::Success,
            award: Award::None,
            update: false,
        }
    }

    /// The ding happened: pay the teams and age the flags.
    pub fn team_award() -> Self {
        Self {
            status: CalcStatus::Success,
            award: Award::Team,
            update: true,
        }
    }
}

/// A scoring algorithm (ASSS `Iturfrewardpoints`). Fill in each team's
/// `num_points` and the arena's `num_points`, then say who to pay.
pub trait TurfRewardPoints: Send + Sync {
    /// Works out what this ding is worth.
    fn calc_reward(&self, zone: &mut Zone, arena: ArenaId, data: &mut TurfData) -> CalcState;
}

/// ASSS `CB_TURFPOSTREWARD`: a ding has been paid out. The data is what the
/// reward was worked out from; it is cleared right after this.
#[derive(Debug, Clone)]
pub struct TurfPostReward {
    /// The arena.
    pub arena: ArenaId,
    /// What the scoring module decided. ASSS hands the whole `TurfArena`
    /// to this callback, `calcState` included.
    pub state: CalcState,
    /// Everything the ding was based on.
    pub data: TurfData,
}

impl Event for TurfPostReward {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

fn data(zone: &mut Zone, arena: ArenaId) -> Option<&mut TurfData> {
    zone.arenas
        .get_mut(arena)
        .and_then(|a| a.ext.get_mut::<TurfData>())
}

fn say_arena(zone: &mut Zone, arena: ArenaId, sound: u8, text: &str) {
    let to: Vec<PlayerId> = zone.players.in_arena(arena).map(|p| p.id).collect();
    if let Some(c) = zone.iface::<dyn Chat>() {
        c.send_any_message(
            zone,
            &to,
            oss_proto::common::chat_type::ARENA,
            sound,
            -1,
            text,
        );
    }
}

fn say_player(zone: &mut Zone, p: PlayerId, sound: u8, text: &str) {
    if let Some(c) = zone.iface::<dyn Chat>() {
        c.send_any_message(
            zone,
            &[p],
            oss_proto::common::chat_type::ARENA,
            sound,
            -1,
            text,
        );
    }
}

/// What a flag is worth right now (ASSS `calcWeight`).
fn weight_of(settings: &TurfSettings, flag: &TurfFlag, now: Ticks) -> i32 {
    let step = match settings.weight_calc {
        WeightCalc::Dings => flag.dings as u64,
        WeightCalc::Time => now.since(flag.tag_tc) / TICKS_PER_SECOND / 60,
    };
    let last = settings.weights.len().saturating_sub(1);
    let i = usize::try_from(step).unwrap_or(last).min(last);
    settings.weights.get(i).copied().unwrap_or(0)
}

/// Sets up the flag array for the arena's map and reads the settings.
fn setup(zone: &mut Zone, arena: ArenaId, keep_flags: bool) {
    let settings = match zone.arenas.get(arena) {
        Some(a) => TurfSettings::load(&a.cfg),
        None => return,
    };
    let count = super::flagcore::flag_count(zone, arena);
    let now = zone.now();
    let Some(a) = zone.arenas.get_mut(arena) else {
        return;
    };
    let d = a.ext.get_or_default::<TurfData>();
    let initial = settings.timer_initial;
    d.settings = settings;
    if !keep_flags || d.flags.len() != count {
        d.flags = vec![TurfFlag::default(); count];
        for f in &mut d.flags {
            f.freq = -1;
        }
    }
    if !keep_flags {
        d.ding_time = now;
        d.next_ding = Ticks(now.0 + initial);
        d.ding_stopped = false;
    }
}

/// Grows or shrinks the flag array to match the arena's flags, keeping
/// what is known about the ones that stay. The flag game creates its flags
/// after the modules attach, and a map change can alter the count, so this
/// runs before anything reads them.
fn sync_flags(zone: &mut Zone, arena: ArenaId) {
    let count = super::flagcore::flag_count(zone, arena);
    if let Some(d) = data(zone, arena)
        && d.flags.len() != count
    {
        d.flags.resize_with(count, || TurfFlag {
            freq: -1,
            ..Default::default()
        });
    }
}

/// A flag was tagged (ASSS `flagTag`): ownership, steals and recoveries.
fn on_tag(zone: &mut Zone, ev: &TurfTag) {
    let now = zone.now();
    sync_flags(zone, ev.arena);
    let name = zone
        .players
        .get(ev.player)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let Some(d) = data(zone, ev.arena) else {
        return;
    };
    let Some(flag) = d.flags.get(ev.fid).cloned() else {
        tracing::warn!(target: "turf_reward", fid = ev.fid, "tag of a flag that isn't there");
        return;
    };
    if flag.freq == ev.new_freq {
        return;
    }
    let settings = d.settings.clone();
    let mut flag = flag;
    let mut lost_by: Option<String> = None;
    if flag.freq >= 0 {
        // the team that lost it may get a chance to tag it back
        if settings.recover_max < 0 || (flag.recovered as i32) < settings.recover_max {
            flag.old.insert(
                0,
                Previous {
                    last_owned: 0,
                    tagger: flag.tagger.clone(),
                    freq: flag.freq,
                    dings: flag.dings,
                    weight: flag.weight,
                    recovered: flag.recovered,
                    tag_tc: flag.tag_tc,
                    lost_tc: now,
                },
            );
        }
        lost_by = Some(flag.tagger.clone());
        d.lost += 1;
        d.steals += 1;
    }
    flag.freq = ev.new_freq;
    flag.tagger = name.clone();
    d.tags += 1;

    // did this team own it recently enough to get it back as it was?
    let mut recovered = false;
    if let Some(i) = flag.old.iter().position(|o| o.freq == ev.new_freq) {
        let o = flag.old[i].clone();
        let too_late = matches!(
            settings.recovery_cutoff,
            RecoveryCutoff::Time | RecoveryCutoff::DingsAndTime
        ) && now.since(o.lost_tc) / TICKS_PER_SECOND > settings.recover_time;
        flag.old.remove(i);
        if !too_late {
            flag.dings = o.dings;
            flag.weight = o.weight;
            flag.recovered = o.recovered + 1;
            flag.tag_tc = o.tag_tc;
            recovered = true;
            d.recoveries += 1;
        }
    }
    if !recovered {
        flag.dings = 0;
        flag.tag_tc = now;
        flag.recovered = 0;
        flag.weight = weight_of(&settings, &flag, now);
    }
    flag.last_tc = now;
    let (steal, dings, weight) = (lost_by.is_some(), flag.dings, flag.weight);
    d.flags[ev.fid] = flag;

    let who = d.players.entry(name).or_default();
    who.tags += 1;
    if steal {
        who.steals += 1;
    }
    if recovered {
        who.recoveries += 1;
    }
    if let Some(lost) = lost_by {
        d.players.entry(lost).or_default().lost += 1;
    }
    tracing::debug!(
        target: "turf_reward",
        fid = ev.fid, dings, weight, freq = ev.new_freq,
        "flag tagged"
    );
}

/// Counts teams, players, flags and weights for this ding (ASSS `preCalc`).
/// Finds a team, making it if it is new. ASSS's `getTeamPtr` pushes a new
/// team on the FRONT of `ta->teams`, so the list ends up in reverse order of
/// first reference, and `validTeams` -- built by walking it -- inherits
/// that. `TR_STYLE_FIXED_PTS` pays by position after a stable sort, so this
/// ordering decides who gets the 1000 when two teams tie: it is not
/// cosmetic. Teams per arena are few, so a scan is cheaper than a map.
fn team_at(teams: &mut Vec<TurfTeam>, freq: i16) -> usize {
    match teams.iter().position(|t| t.freq == freq) {
        Some(i) => i,
        None => {
            teams.insert(
                0,
                TurfTeam {
                    freq,
                    ..Default::default()
                },
            );
            0
        }
    }
}

fn pre_calc(zone: &mut Zone, arena: ArenaId) {
    let now = zone.now();
    // ASSS walks every player whose arena is this one, whatever their
    // status, and counts the ones still entering into numPlayers while
    // leaving them out of numValidPlayers. `in_arena` would drop them, and
    // with them their weight against MinPlayersArena.
    let here: Vec<(PlayerId, String, u16, bool, bool, bool)> = zone
        .players
        .iter()
        .filter(|p| p.arena == Some(arena))
        .map(|p| {
            (
                p.id,
                p.name.clone(),
                p.freq,
                p.status == PlayerStatus::Playing,
                p.ship.is_spec(),
                p.pos.status & SAFE_ZONE != 0,
            )
        })
        .collect();
    let Some(d) = data(zone, arena) else { return };
    let settings = d.settings.clone();
    let num_flags = d.flags.len() as f64;

    // weights, and each team's share of them
    d.num_weights = 0;
    let mut teams: Vec<TurfTeam> = Vec::new();
    for i in 0..d.flags.len() {
        if d.flags[i].freq < 0 {
            continue;
        }
        let w = weight_of(&settings, &d.flags[i], now);
        d.flags[i].weight = w;
        let freq = d.flags[i].freq;
        let at = team_at(&mut teams, freq);
        let t = &mut teams[at];
        t.num_flags += 1;
        t.num_weights += i64::from(w);
        d.num_weights += i64::from(w);
    }
    for t in &mut teams {
        t.percent_flags = if num_flags > 0.0 {
            f64::from(t.num_flags) / num_flags * 100.0
        } else {
            0.0
        };
        t.percent_weights = if d.num_weights == 0 {
            0.0
        } else {
            t.num_weights as f64 / d.num_weights as f64 * 100.0
        };
    }

    // players, split into those who are paid and those who are only here
    d.num_players = 0;
    d.num_valid_players = 0;
    for (id, name, freq, playing, spec, safe) in here {
        let freq = freq as i16;
        let at = team_at(&mut teams, freq);
        let t = &mut teams[at];
        d.num_players += 1;
        d.players.entry(name).or_default();
        if !playing
            || (!settings.spec_receive_points && spec)
            || (!settings.safe_receive_points && safe)
        {
            t.players_no_pts.push(id);
        } else {
            t.players_pts.push(id);
            t.num_players += 1;
            d.num_valid_players += 1;
        }
    }

    // which teams qualify
    d.sum_per_capita_flags = 0.0;
    d.sum_per_capita_weights = 0.0;
    d.num_teams = 0;
    d.num_valid_teams = 0;
    let mut list = teams;
    list.retain(|t| t.num_players > 0 || t.num_flags > 0 || !t.players_no_pts.is_empty());
    for t in &mut list {
        d.num_teams += 1;
        t.valid = i32::try_from(t.num_players).unwrap_or(i32::MAX) >= settings.min_players_on_team
            && i32::try_from(t.num_flags).unwrap_or(i32::MAX) >= settings.min_flags
            && t.percent_flags >= settings.min_percent_flags
            && t.num_weights >= settings.min_weights
            && t.percent_weights >= settings.min_percent_weights;
        if t.valid {
            d.num_valid_teams += 1;
            if t.num_players > 0 {
                // ASSS divides int/int and long/int here and only widens the
                // result, so seven weights over three players is 2, not
                // 2.333: `turf_reward.c:1281`. Every percentage downstream
                // inherits the truncation, while `percent_weights` just
                // above is a true ratio -- the module really does mix the
                // two in one calculation, and a port that tidies that up
                // pays the wrong teams.
                // checked_div rather than div_euclid: C truncates towards
                // zero, so a configured negative weight must not round the
                // other way. num_players is non-zero here.
                t.per_capita_flags = f64::from(t.num_flags.checked_div(t.num_players).unwrap_or(0));
                t.per_capita_weights = t
                    .num_weights
                    .checked_div(i64::from(t.num_players))
                    .unwrap_or(0) as f64;
                d.sum_per_capita_flags += t.per_capita_flags;
                d.sum_per_capita_weights += t.per_capita_weights;
            }
        }
    }
    d.teams = list;
    d.num_points = 0;
}

/// Flags age by one ding, and recovery windows close (ASSS `updateFlags`).
fn update_flags(d: &mut TurfData, now: Ticks) {
    let settings = d.settings.clone();
    for flag in &mut d.flags {
        if flag.freq >= 0 {
            flag.dings += 1;
            flag.weight = weight_of(&settings, flag, now);
        }
        for o in &mut flag.old {
            o.last_owned += 1;
        }
        let by_dings = matches!(
            settings.recovery_cutoff,
            RecoveryCutoff::Dings | RecoveryCutoff::DingsAndTime
        );
        let by_time = matches!(
            settings.recovery_cutoff,
            RecoveryCutoff::Time | RecoveryCutoff::DingsAndTime
        );
        flag.old.retain(|o| {
            if by_dings && o.last_owned > settings.recover_dings {
                return false;
            }
            if by_time && now.since(o.lost_tc) / TICKS_PER_SECOND > settings.recover_time {
                return false;
            }
            true
        });
    }
}

/// Pays out and tells each player what they got.
fn award(zone: &mut Zone, d: &mut TurfData, state: CalcState) {
    let Some(stats) = zone.iface::<dyn Stats>() else {
        return;
    };
    let max = d.settings.max_points;
    let jackpot = d.num_points;
    let teams = d.teams.clone();
    for team in &teams {
        if !team.valid {
            for &p in team.players_pts.iter().chain(&team.players_no_pts) {
                say_player(
                    zone,
                    p,
                    SOUND_DING,
                    "Reward: 0/0 (minimum team requirements not met)",
                );
            }
            continue;
        }
        for &p in &team.players_pts {
            let name = zone
                .players
                .get(p)
                .map(|x| x.name.clone())
                .unwrap_or_default();
            let own = d.players.get(&name).map_or(0, |s| s.points);
            let points = match state.award {
                Award::Player => own,
                Award::Team => team.num_points,
                Award::Both => own + team.num_points,
                Award::None => 0,
            };
            let points = points.clamp(0, max);
            let tail = if points == max { " [MAX] " } else { "" };
            let text = match state.award {
                Award::Both => format!(
                    "Reward: {points}/{jackpot}{tail} [{own}:{}]",
                    team.num_points
                ),
                _ => format!("Reward: {points}/{jackpot}{tail}"),
            };
            say_player(zone, p, SOUND_DING, &text);
            if points > 0 {
                stats.increment(zone, p, stat::FLAG_POINTS, points);
            }
        }
        for &p in &team.players_no_pts {
            say_player(
                zone,
                p,
                SOUND_DING,
                &format!("Reward: 0/{jackpot} (not playing)"),
            );
        }
    }
    stats.send_updates(zone, None);
}

/// One ding (ASSS `doReward`).
pub fn do_reward(zone: &mut Zone, arena: ArenaId) {
    if data(zone, arena).is_none() {
        return;
    }
    sync_flags(zone, arena);
    pre_calc(zone, arena);
    let now = zone.now();
    // the data is taken out while the scoring module works on it, so it
    // can't be reached through the arena at the same time
    let Some(mut d) = zone
        .arenas
        .get_mut(arena)
        .and_then(|a| a.ext.get_mut::<TurfData>())
        .map(std::mem::take)
    else {
        return;
    };
    let enough_players = d.num_players >= d.settings.min_players_in_arena;
    let enough_teams = d.num_valid_teams >= d.settings.min_teams;
    if !enough_players || !enough_teams {
        say_arena(
            zone,
            arena,
            SOUND_DING,
            "Reward: 0 (arena minimum requirements not met)",
        );
        tracing::debug!(
            target: "turf_reward",
            arena = arena.0, players = d.num_players, teams = d.num_valid_teams,
            "arena minimums not met"
        );
    } else if let Some(points) = zone.iface_in::<dyn TurfRewardPoints>(arena) {
        let state = points.calc_reward(zone, arena, &mut d);
        if state.status == CalcStatus::Fail {
            // ASSS returns out of doReward here, before the post-reward
            // callback and before anything is paid or aged, and tells its
            // timer to stop: `TR_STYLE_DISABLED` halts an arena's dings
            // rather than running them for nothing.
            tracing::debug!(target: "turf_reward", arena = arena.0, "scoring failed, halting further rewards");
            d.ding_stopped = true;
            if let Some(a) = zone.arenas.get_mut(arena) {
                a.ext.insert(d);
            }
            return;
        }
        if state.award != Award::None {
            award(zone, &mut d, state);
        }
        zone.fire(&TurfPostReward {
            arena,
            state,
            data: d.clone(),
        });
        if state.update {
            update_flags(&mut d, now);
        }
    } else {
        tracing::warn!(target: "turf_reward", arena = arena.0, "no scoring module attached");
    }
    // clear the counters for the next round, keeping the flags
    d.ding_time = now;
    d.next_ding = Ticks(now.0 + d.settings.timer_interval);
    d.tags = 0;
    d.steals = 0;
    d.recoveries = 0;
    d.lost = 0;
    d.kills = 0;
    d.bounty_exchanged = 0;
    d.num_points = 0;
    d.teams.clear();
    d.players.clear();
    if let Some(a) = zone.arenas.get_mut(arena) {
        a.ext.insert(d);
    }
}

fn tick(zone: &mut Zone) -> bool {
    let now = zone.now();
    for arena in zone.arenas.ids() {
        let due = zone
            .arenas
            .get(arena)
            .and_then(|a| a.ext.get::<TurfData>())
            .filter(|d| !d.ding_stopped)
            .map(|d| d.next_ding);
        if due.is_some_and(|t| now.0 >= t.0) {
            do_reward(zone, arena);
        }
    }
    true
}

// ---- commands -------------------------------------------------------------

fn c_turftime(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let now = zone.now();
    let Some(left) = zone
        .arenas
        .get(arena)
        .and_then(|a| a.ext.get::<TurfData>())
        .map(|d| d.next_ding.0.saturating_sub(now.0) / TICKS_PER_SECOND)
    else {
        return;
    };
    let (d, h, m, s) = (left / 86400, left / 3600 % 24, left / 60 % 60, left % 60);
    let text = if d > 0 {
        format!("Next ding in: {d} days {h} hours {m} minutes {s} seconds.")
    } else if h > 0 {
        format!("Next ding in: {h} hours {m} minutes {s} seconds.")
    } else if m > 0 {
        format!("Next ding in: {m} minutes {s} seconds.")
    } else {
        format!("Next ding in: {s} seconds.")
    };
    message(zone, inv.sender, &text);
}

fn c_turfinfo(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let Some(d) = zone
        .arenas
        .get(arena)
        .and_then(|a| a.ext.get::<TurfData>())
        .cloned()
    else {
        return;
    };
    let s = &d.settings;
    let lines = [
        format!(
            "Reward style: {:?}, modifier {}, max {} points a ding",
            s.reward_style, s.reward_modifier, s.max_points
        ),
        format!(
            "Minimums: {} players on a team, {} in the arena, {} teams, {} flags, {} weights",
            s.min_players_on_team, s.min_players_in_arena, s.min_teams, s.min_flags, s.min_weights
        ),
        format!(
            "Weights: {:?}, {} set; recovery by {:?} ({} dings, {} seconds, max {})",
            s.weight_calc,
            s.weights.len(),
            s.recovery_cutoff,
            s.recover_dings,
            s.recover_time,
            s.recover_max
        ),
        format!(
            "Flags: {} on the map, {} weights owned",
            d.flags.len(),
            d.num_weights
        ),
    ];
    for l in lines {
        message(zone, inv.sender, &l);
    }
}

fn c_forceding(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    // ASSS leaves the timer alone, so the next ding comes when it would have
    let next = zone
        .arenas
        .get(arena)
        .and_then(|a| a.ext.get::<TurfData>())
        .map(|d| d.next_ding);
    do_reward(zone, arena);
    if let (Some(next), Some(d)) = (next, data(zone, arena)) {
        d.next_ding = next;
    }
}

fn c_turfresetflags(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let Some(d) = data(zone, arena) else { return };
    for f in &mut d.flags {
        *f = TurfFlag {
            freq: -1,
            ..Default::default()
        };
    }
    say_arena(zone, arena, SOUND_DING, "Notice: Turf flag data reset.");
}

fn c_turfresettimer(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(arena) = inv.arena else { return };
    let now = zone.now();
    if let Some(d) = data(zone, arena) {
        d.ding_time = now;
        d.next_ding = Ticks(now.0 + d.settings.timer_interval);
        d.ding_stopped = false;
    }
    message(zone, inv.sender, "Turf timer reset.");
}

/// The `turf_reward` module (attach to arenas).
pub struct TurfRewardModule;

impl Module for TurfRewardModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.set_timer(me, TICKS_PER_SECOND, TICKS_PER_SECOND, tick);
        let help = |args: &str, what: &str| {
            format!("Module: turf_reward\nTargets: none\nArgs: {args}\n{what}\n")
        };
        zone.add_command(
            me,
            "turftime",
            None,
            Some(&help("none", "Says how long until the next ding.")),
            c_turftime,
        );
        zone.add_command(
            me,
            "turfinfo",
            None,
            Some(&help("none", "Shows this arena's turf reward settings.")),
            c_turfinfo,
        );
        zone.add_command(
            me,
            "forceding",
            None,
            Some(&help(
                "none",
                "Dings now, without moving the timer for the next one.",
            )),
            c_forceding,
        );
        zone.add_command(
            me,
            "turfresetflags",
            None,
            Some(&help(
                "none",
                "Forgets every flag's owner, weight and recovery history.",
            )),
            c_turfresetflags,
        );
        zone.add_command(
            me,
            "turfresettimer",
            None,
            Some(&help("none", "Starts the ding timer again from now.")),
            c_turfresettimer,
        );
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        setup(zone, arena, false);
        zone.on::<TurfTag>(me, Scope::Arena(arena), on_tag);
        zone.on::<Kill>(me, Scope::Arena(arena), |z, ev| {
            let bounty = i64::from(ev.bounty);
            let (killer, killed) = (
                z.players.get(ev.killer).map(|p| p.name.clone()),
                z.players.get(ev.killed).map(|p| p.name.clone()),
            );
            if let Some(d) = data(z, ev.arena) {
                d.kills += 1;
                d.bounty_exchanged += bounty;
                if let Some(n) = killer {
                    let s = d.players.entry(n).or_default();
                    s.kills += 1;
                    s.bounty_taken += u32::try_from(bounty).unwrap_or(0);
                }
                if let Some(n) = killed {
                    d.players.entry(n).or_default().deaths += 1;
                }
            }
        });
        zone.on::<ArenaAction>(me, Scope::Arena(arena), |z, ev| {
            if ev.action == ArenaActionKind::ConfChanged {
                setup(z, ev.arena, true);
            }
        });
        Ok(())
    }
}

// ---- points_turf_reward ---------------------------------------------------

/// ASSS's built-in scoring algorithms.
struct BuiltinPoints;

impl BuiltinPoints {
    /// The weighted algorithm: a jackpot split by each team's weights per
    /// player, so the bigger a team is the less each of its players earns.
    fn standard(&self, zone: &mut Zone, arena: ArenaId, d: &mut TurfData) -> Option<CalcState> {
        if d.flags.is_empty() {
            tracing::warn!(target: "points_turf_reward", arena = arena.0, "map has no flags");
            return None;
        }
        if d.num_weights < 1 || d.sum_per_capita_weights <= 0.0 {
            say_arena(zone, arena, 0, "Notice: All flags are unowned.");
            say_arena(
                zone,
                arena,
                SOUND_DING,
                "Reward:0 (arena minimum requirements not met)",
            );
            return None;
        }
        d.num_points = if d.settings.reward_style == RewardStyle::StdBty {
            d.bounty_exchanged.max(0)
        } else {
            // ASSS multiplies two ints and stores the result in an
            // `unsigned long`, so a negative RewardModifier does not make a
            // negative pot -- it wraps. -200 across four players is
            // 4294966496, and every team is then paid a share of that.
            i64::from(
                d.settings
                    .reward_modifier
                    .wrapping_mul(d.num_players)
                    .cast_unsigned(),
            )
        };
        // ASSS's "no points to award" branch uses CALC_REWARD_SUCCESS,
        // which -- unlike CALC_REWARD_FAILED -- does not end in a `return`.
        // So a zero pot is not an early exit: it falls straight through and
        // still fills in every team's percent before multiplying nothing by
        // it. Teams come out of a bountyless STD_BTY ding with percent 50
        // and numPoints 0.
        let (jackpot, sum) = (d.num_points as f64, d.sum_per_capita_weights);
        for t in d.teams.iter_mut().filter(|t| t.valid) {
            t.percent = t.per_capita_weights / sum * 100.0;
            if t.num_players > 0 {
                let share = jackpot * (t.percent / 100.0) / f64::from(t.num_players);
                t.num_points = share as i32;
            }
        }
        Some(CalcState::team_award())
    }
}

impl TurfRewardPoints for BuiltinPoints {
    fn calc_reward(&self, zone: &mut Zone, arena: ArenaId, d: &mut TurfData) -> CalcState {
        match d.settings.reward_style {
            RewardStyle::Disabled => CalcState::failed(),
            RewardStyle::Other => CalcState::no_award(),
            RewardStyle::Standard | RewardStyle::StdBty => self
                .standard(zone, arena, d)
                .unwrap_or_else(CalcState::failed),
            RewardStyle::Periodic => {
                // crStandard's failure paths return out of crStandard, not
                // out of crPeriodic, so whatever it decided is discarded
                // here and the periodic formula runs regardless.
                let _ = self.standard(zone, arena, d);
                let modifier = d.settings.reward_modifier;
                let players = d.num_players;
                for t in d.teams.iter_mut().filter(|t| t.valid) {
                    t.percent = 0.0;
                    t.num_points = match modifier {
                        0 => 0,
                        m if m > 0 => m.saturating_mul(t.num_flags as i32),
                        m => players
                            .saturating_mul(-m)
                            .saturating_mul(t.num_flags as i32),
                    };
                }
                // A modifier of zero means "dings disabled", and ASSS does
                // set award NONE for it -- then falls into the `modifier > 0`
                // else-branch anyway (zero is not greater than zero),
                // recomputes the same zero, and ends on CALC_REWARD_SUCCESS,
                // which puts award back to TEAM. So a disabled periodic ding
                // still pays, through the award path, nothing at all.
                CalcState::team_award()
            }
            RewardStyle::Weights => {
                for t in d.teams.iter_mut().filter(|t| t.valid) {
                    t.percent = 0.0;
                    t.num_points = i32::try_from(t.num_weights.max(0)).unwrap_or(i32::MAX);
                }
                CalcState::team_award()
            }
            RewardStyle::FixedPts => {
                if self.standard(zone, arena, d).is_none() {
                    return CalcState::failed();
                }
                // ASSS sorts validTeams with a comparator that is a strict
                // less-than on numPoints and LLSort is a stable merge sort,
                // so the list comes out ASCENDING: the 1000 goes to the team
                // the weighted algorithm rated LOWEST. The source carries a
                // "CHECK: I am unsure if LLSort sorts asc or desc" comment at
                // that line; the vectors settle it. Ties keep validTeams
                // order, which `team_at` reproduces.
                let mut order: Vec<usize> =
                    (0..d.teams.len()).filter(|&i| d.teams[i].valid).collect();
                order.sort_by_key(|&i| d.teams[i].num_points);
                let mut pts = 1000;
                for i in order {
                    d.teams[i].percent = 0.0;
                    d.teams[i].num_points = pts.max(0);
                    if pts >= 0 {
                        pts -= 500;
                    }
                }
                CalcState::team_award()
            }
        }
    }
}

/// The `points_turf_reward` module (attach to arenas, with `turf_reward`).
pub struct PointsTurfRewardModule;

impl Module for PointsTurfRewardModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn TurfRewardPoints>(me, Scope::Arena(arena), Arc::new(BuiltinPoints));
        Ok(())
    }
}
