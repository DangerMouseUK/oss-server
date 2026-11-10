//! `points_goal`: soccer scoring (attach to arenas; ASSS
//! `scoring/points_goal.c`).
//!
//! `Soccer:Mode` decides who a goal counts for: `GOAL_ALL` (the scorer's
//! team), `GOAL_LEFTRIGHT` / `GOAL_TOPBOTTOM` (map halves, two teams), and
//! the four-team corner/side modes. With positive `Soccer:CapturePoints`
//! each team starts with that many points and goals steal them; with a
//! negative value teams count goals up to `-CapturePoints` (and must lead by
//! `Soccer:WinBy`). The winners get `Soccer:Reward` (negative = flat points,
//! positive = players² × reward / 1000) as flag points, subject to
//! `Soccer:MinPlayers`/`MinTeams`. Commands: `?score`, `?setscore`,
//! `?resetgame`.

use crate::arena::ArenaId;
use crate::commands::Invocation;
use crate::module::{Module, ModuleError};
use crate::player::PlayerId;
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::balls::Goal;
use super::chat::Chat;
use super::stats::{Stats, stat};

/// Teams tracked (ASSS `CFG_SOCCER_MAXFREQ`).
pub const MAX_FREQ: usize = 8;
/// ASSS `SOUND_DING`.
const SOUND_DING: u8 = 103;
/// ASSS `SOUND_GOAL`.
const SOUND_GOAL: u8 = 104;

/// Goal configuration (`Soccer:Mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum GoalMode {
    /// Any goal, for the scorer's team.
    #[default]
    All,
    /// Left half / right half.
    LeftRight,
    /// Top half / bottom half.
    TopBottom,
    /// Quadrants, defend one goal.
    Corners3_1,
    /// Quadrants, defend three goals.
    Corners1_3,
    /// Sides, defend one goal.
    Sides3_1,
    /// Sides, defend three goals.
    Sides1_3,
}

impl GoalMode {
    fn from_cfg(s: Option<&str>) -> Self {
        let s = s.unwrap_or("").trim().to_ascii_uppercase();
        match s.as_str() {
            "GOAL_LEFTRIGHT" => Self::LeftRight,
            "GOAL_TOPBOTTOM" => Self::TopBottom,
            "GOAL_CORNERS_3_1" => Self::Corners3_1,
            "GOAL_CORNERS_1_3" => Self::Corners1_3,
            "GOAL_SIDES_3_1" => Self::Sides3_1,
            "GOAL_SIDES_1_3" => Self::Sides1_3,
            _ => Self::All,
        }
    }

    fn two_team(self) -> bool {
        matches!(self, Self::All | Self::LeftRight | Self::TopBottom)
    }
}

#[derive(Debug, Default)]
struct Scores {
    mode: GoalMode,
    steal: i32,
    capture: i32,
    reward: i32,
    win_by: i32,
    /// How many times this arena's game has been ended here.
    end_games: u32,
    score: [i32; MAX_FREQ],
}

fn scores(zone: &mut Zone, a: ArenaId) -> Option<&mut Scores> {
    zone.arenas.get_mut(a)?.ext.get_mut::<Scores>()
}

fn say(zone: &mut Zone, to: &[PlayerId], sound: u8, text: &str) {
    if let Some(c) = zone.iface::<dyn Chat>() {
        c.send_any_message(
            zone,
            to,
            oss_proto::common::chat_type::ARENA,
            sound,
            -1,
            text,
        );
    }
}

fn say_arena(zone: &mut Zone, a: ArenaId, sound: u8, text: &str) {
    let to: Vec<PlayerId> = zone.players.in_arena(a).map(|p| p.id).collect();
    say(zone, &to, sound, text);
}

fn score_msg(zone: &mut Zone, a: ArenaId, to: Option<PlayerId>) {
    let Some(s) = scores(zone, a) else { return };
    if s.mode == GoalMode::All {
        return;
    }
    let (mode, sc) = (s.mode, s.score);
    let ships = zone
        .arenas
        .get(a)
        .is_some_and(|x| x.cfg.get_int("Misc", "FrequencyShipTypes", 0) != 0);
    let msg = if !mode.two_team() {
        if ships {
            format!(
                "SCORE: Warbirds:{} Javelins:{} Spiders:{} Leviathans:{}",
                sc[0], sc[1], sc[2], sc[3]
            )
        } else {
            format!(
                "SCORE: Team0:{} Team1:{} Team2:{} Team3:{}",
                sc[0], sc[1], sc[2], sc[3]
            )
        }
    } else if ships {
        format!("SCORE: Warbirds:{} Javelins:{}", sc[0], sc[1])
    } else {
        format!("SCORE: Evens:{} Odds:{}", sc[0], sc[1])
    };
    match to {
        Some(p) => say(zone, &[p], 0, &msg),
        None => say_arena(zone, a, 0, &msg),
    }
}

/// ASSS `RewardPoints`: gives the winners their reward; returns it.
/// ASSS `RewardPoints`: pays the winning freq and returns what each of its
/// players got.
pub fn reward_points(zone: &mut Zone, a: ArenaId, win_freq: i16) -> i32 {
    let reward = scores(zone, a).map_or(0, |s| s.reward);
    let (mut players, mut freqs) = (0i64, [0u32; 4]);
    let mut winners = Vec::new();
    let mut members = Vec::new();
    for p in zone.players.in_arena(a) {
        if p.ship.is_spec() {
            continue;
        }
        players += 1;
        freqs[usize::from(p.freq % 4)] += 1;
        let won = p.freq as i16 == win_freq;
        members.push((p.id, won));
        if won && p.pos.status & oss_proto::position::status::SAFEZONE == 0 {
            winners.push(p.id);
        }
    }
    let st = zone.iface::<dyn Stats>();
    if let Some(st) = &st {
        for (pid, won) in &members {
            st.increment(
                zone,
                *pid,
                if *won {
                    stat::BALL_GAMES_WON
                } else {
                    stat::BALL_GAMES_LOST
                },
                1,
            );
        }
    }
    let mut points = if reward < 1 {
        -i64::from(reward)
    } else {
        players * players * i64::from(reward) / 1000
    };
    let teams = freqs.iter().filter(|&&n| n > 0).count() as i32;
    let (min_players, min_teams) = zone.arenas.get(a).map_or((0, 0), |x| {
        (
            x.cfg.get_int("Soccer", "MinPlayers", 0),
            x.cfg.get_int("Soccer", "MinTeams", 0),
        )
    });
    if players < i64::from(min_players) || teams < min_teams {
        points = 0;
    } else if let Some(st) = &st {
        for pid in winners {
            st.increment(zone, pid, stat::FLAG_POINTS, points as i32);
        }
    }
    points.clamp(0, i64::from(i32::MAX)) as i32
}

fn game_over(zone: &mut Zone, a: ArenaId, freq: usize, reset_to: i32) {
    say_arena(zone, a, SOUND_DING, "Soccer game over.");
    reward_points(zone, a, freq as i16);
    super::balls::end_game(zone, a);
    if let Some(s) = scores(zone, a) {
        s.score = [reset_to; MAX_FREQ];
        s.end_games += 1;
    }
}

/// The arena's eight per-freq scores.
pub fn score(zone: &mut Zone, a: ArenaId) -> Option<[i32; MAX_FREQ]> {
    scores(zone, a).map(|s| s.score)
}

/// How many soccer games this arena has ended.
pub fn end_games(zone: &mut Zone, a: ArenaId) -> u32 {
    scores(zone, a).map_or(0, |s| s.end_games)
}

/// ASSS `CheckGameOver`.
fn check_game_over(zone: &mut Zone, a: ArenaId) {
    let Some(s) = scores(zone, a) else { return };
    let mut freq = 0;
    for i in 0..MAX_FREQ {
        if s.score[i] > s.score[freq] {
            freq = i;
        }
    }
    let (mode, steal, cap, by, sc) = (s.mode, s.steal, s.capture, s.win_by, s.score);
    if mode <= GoalMode::TopBottom && steal != 0 {
        if sc[1 - freq.min(1)] == 0 {
            game_over(zone, a, freq, steal);
        }
    } else if mode > GoalMode::TopBottom && steal != 0 {
        if sc[..4].iter().filter(|&&v| v == 0).count() == 3 {
            game_over(zone, a, freq, steal);
        }
    } else {
        let mut j = 0;
        if sc[freq] >= -cap {
            // The leader is counted here too, as in ASSS, and that is the
            // whole behaviour of `Soccer:WinBy 0` -- the default. The
            // leader's own `score + 0 <= score` holds, so j reaches 8 and
            // can never equal 7: an absolute-scoring game with WinBy 0
            // never ends, however far ahead someone gets. It takes WinBy 1
            // or more, which excludes the leader by arithmetic rather than
            // by a special case, and leaves the other seven freqs each at
            // least WinBy behind. Skipping the leader here would look
            // tidier and would end games a zone's own settings say should
            // run forever.
            j = (0..MAX_FREQ).filter(|&i| sc[i] + by <= sc[freq]).count();
        }
        if j == MAX_FREQ - 1 {
            game_over(zone, a, freq, 0);
        }
    }
}

fn on_goal(zone: &mut Zone, g: &Goal) {
    let a = g.arena;
    let ball_freq = super::balls::balls(zone, a)
        .get(g.ball)
        .map_or(-1, |b| b.freq);
    let Some(s) = scores(zone, a) else { return };
    let mut null_goal = false;
    let quadrant = |x: i32, y: i32| {
        if x < 512 {
            if y < 512 { 0 } else { 2 }
        } else if y < 512 {
            1
        } else {
            3
        }
    };
    let (x, y) = (i32::from(g.x), i32::from(g.y));
    let freq: i16 = match s.mode {
        GoalMode::All => ball_freq,
        GoalMode::LeftRight | GoalMode::TopBottom => {
            let f = if (s.mode == GoalMode::LeftRight && x < 512)
                || (s.mode == GoalMode::TopBottom && y < 512)
            {
                1
            } else {
                0
            };
            s.score[f] += 1;
            if s.steal != 0 {
                s.score[1 - f] -= 1;
            }
            f as i16
        }
        GoalMode::Corners3_1 | GoalMode::Sides3_1 => {
            let i = if s.mode == GoalMode::Corners3_1 {
                quadrant(x, y)
            } else if x < y {
                if x < 1024 - y { 0 } else { 1 }
            } else if x < 1024 - y {
                2
            } else {
                3
            };
            let f = usize::try_from(ball_freq).unwrap_or(0).min(MAX_FREQ - 1);
            if s.steal == 0 {
                s.score[f] += 1;
            } else if s.score[i] != 0 {
                s.score[f] += 1;
                s.score[i] -= 1;
            } else {
                null_goal = true;
            }
            ball_freq
        }
        GoalMode::Corners1_3 | GoalMode::Sides1_3 => {
            let f = usize::try_from(ball_freq).unwrap_or(0).min(MAX_FREQ - 1);
            s.score[f] += 1;
            ball_freq
        }
    };
    let (reward, mode) = (s.reward, s.mode);
    let name = zone
        .players
        .get(g.player)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let (team, enemy): (Vec<PlayerId>, Vec<PlayerId>) = {
        let all: Vec<(PlayerId, bool)> = zone
            .players
            .in_arena(a)
            .map(|p| (p.id, p.freq as i16 == freq))
            .collect();
        let team = all.iter().filter(|(_, t)| *t).map(|(p, _)| *p).collect();
        let enemy = all.iter().filter(|(_, t)| !*t).map(|(p, _)| *p).collect();
        (team, enemy)
    };
    if reward != 0 {
        let pts = reward_points(zone, a, freq);
        say(
            zone,
            &team,
            SOUND_GOAL,
            &format!("Team Goal! by {name}  Reward:{pts}"),
        );
        say(
            zone,
            &enemy,
            SOUND_GOAL,
            &format!("Enemy Goal! by {name}  Reward:{pts}"),
        );
        if let Some(st) = zone.iface::<dyn Stats>() {
            st.send_updates(zone, None);
        }
    } else {
        say(zone, &team, SOUND_GOAL, &format!("Team Goal! by {name}"));
        say(zone, &enemy, SOUND_GOAL, &format!("Enemy Goal! by {name}"));
        if null_goal {
            say_arena(zone, a, 0, "Enemy goal had no points to give.");
        }
    }
    if mode != GoalMode::All {
        score_msg(zone, a, None);
        check_game_over(zone, a);
    }
}

fn c_score(zone: &mut Zone, inv: &Invocation<'_>) {
    if let Some(a) = inv.arena {
        score_msg(zone, a, Some(inv.sender));
    }
}

fn c_setscore(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let vals: Vec<i32> = inv
        .params
        .split_whitespace()
        .map_while(|v| v.parse().ok())
        .take(MAX_FREQ)
        .collect();
    if vals.is_empty() {
        super::chat::message(
            zone,
            inv.sender,
            "setscore format: *setscore x y z .... where x = freq 0, y = 1,etc",
        );
        return;
    }
    let Some(s) = scores(zone, a) else { return };
    if s.mode == GoalMode::All || s.steal != 0 {
        return;
    }
    for (i, v) in vals.into_iter().enumerate() {
        if v == -1 {
            break;
        }
        s.score[i] = v;
    }
    score_msg(zone, a, None);
    check_game_over(zone, a);
}

fn c_resetgame(zone: &mut Zone, inv: &Invocation<'_>) {
    let Some(a) = inv.arena else { return };
    let Some((mode, steal)) = scores(zone, a).map(|s| (s.mode, s.steal)) else {
        return;
    };
    if mode == GoalMode::All {
        return;
    }
    let name = zone
        .players
        .get(inv.sender)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    say_arena(zone, a, 0, &format!("Resetting game. -{name}"));
    say_arena(zone, a, SOUND_DING, "Soccer game over.");
    super::balls::end_game(zone, a);
    if let Some(s) = scores(zone, a) {
        s.score = [steal; MAX_FREQ];
    }
}

/// The `points_goal` module (attach to arenas).
pub struct PointsGoalModule;

impl Module for PointsGoalModule {
    fn load(&self, _zone: &mut Zone, _me: Owner) -> Result<(), ModuleError> {
        Ok(())
    }

    fn attach(&self, zone: &mut Zone, me: Owner, arena: ArenaId) -> Result<(), ModuleError> {
        let Some(cfg) = zone.arenas.get(arena).map(|a| a.cfg.clone()) else {
            return Ok(());
        };
        let capture = cfg.get_int("Soccer", "CapturePoints", 1);
        let steal = capture.max(0);
        let s = Scores {
            mode: GoalMode::from_cfg(cfg.get("Soccer", "Mode")),
            steal,
            capture,
            reward: cfg.get_int("Soccer", "Reward", 0),
            win_by: cfg.get_int("Soccer", "WinBy", 0),
            score: [steal; MAX_FREQ],
            end_games: 0,
        };
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.insert(s);
        }
        zone.on::<Goal>(me, Scope::Arena(arena), on_goal);
        zone.add_command(me, "score", Some(arena), Some("Module: points_goal\nTargets: none\nArgs: none\nReturns current score of the soccer game in progress.\n"), c_score);
        zone.add_command(me, "setscore", Some(arena), Some("Module: points_goal\nTargets: none\nArgs: <freq 0 score> [<freq 1 score> ...]\nChanges the score of the current soccer game (absolute scoring only).\n"), c_setscore);
        zone.add_command(me, "resetgame", Some(arena), Some("Module: points_goal\nTargets: none\nArgs: none\nResets soccer game scores and balls.\n"), c_resetgame);
        Ok(())
    }

    fn detach(&self, zone: &mut Zone, _me: Owner, arena: ArenaId) {
        if let Some(a) = zone.arenas.get_mut(arena) {
            a.ext.remove::<Scores>();
        }
    }
}
