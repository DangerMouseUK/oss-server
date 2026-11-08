//! The client settings packet (`S2C 0x0F`), built from an arena's config
//! exactly as ASSS 1.4.4 `clientset.c` builds it.
//!
//! The packet is kept as its 1428 wire bytes. Every field is addressed by a
//! little-endian bit offset, which is also how ASSS's *override keys* work
//! (`offset_in_bits | bit_length << 16`), so arena and per-player overrides,
//! the settings checksum and the packet itself all share one representation.
//!
//! Field names, order and quirks come from `clientset.def`/`clientset.c`:
//! values are read with ASSS `GetInt` semantics, truncated to their field
//! width, five damage settings are multiplied by 1000, ship short setting 10
//! holds a bitfield (`SeeBombLevel`, `DisableFastShooting`, `Radius`), and the
//! bit flags in the header read `Soccer:DisableWallPass` while their override
//! keys are named `Misc:DisableBallThroughWalls`/`Misc:DisableBallKilling`.

use crate::config::ConfigFile;

/// Size of the settings packet in bytes, including the type byte.
pub const SETTINGS_LEN: usize = 1428;
/// Packet type byte of the settings packet.
pub const S2C_SETTINGS: u8 = 0x0F;

/// Setting names, in wire order (from ASSS `clientset.def`).
pub mod names {
    /// Ship section names, in ship order.
    pub const SHIP_NAMES: [&str; 8] = [
        "Warbird",
        "Javelin",
        "Spider",
        "Leviathan",
        "Terrier",
        "Weasel",
        "Lancaster",
        "Shark",
    ];

    /// Zone-wide 32-bit settings.
    pub const LONG_NAMES: [&str; 20] = [
        "Bullet:BulletDamageLevel",
        "Bomb:BombDamageLevel",
        "Bullet:BulletAliveTime",
        "Bomb:BombAliveTime",
        "Misc:DecoyAliveTime",
        "Misc:SafetyLimit",
        "Misc:FrequencyShift",
        "Team:MaxFrequency",
        "Repel:RepelSpeed",
        "Mine:MineAliveTime",
        "Burst:BurstDamageLevel",
        "Bullet:BulletDamageUpgrade",
        "Flag:FlagDropDelay",
        "Flag:EnterGameFlaggingDelay",
        "Rocket:RocketThrust",
        "Rocket:RocketSpeed",
        "Shrapnel:InactiveShrapDamage",
        "Wormhole:SwitchTime",
        "Misc:ActivateAppShutdownTime",
        "Shrapnel:ShrapnelSpeed",
    ];

    /// Zone-wide 16-bit settings.
    pub const SHORT_NAMES: [&str; 58] = [
        "Latency:SendRoutePercent",
        "Bomb:BombExplodeDelay",
        "Misc:SendPositionDelay",
        "Bomb:BombExplodePixels",
        "Prize:DeathPrizeTime",
        "Bomb:JitterTime",
        "Kill:EnterDelay",
        "Prize:EngineShutdownTime",
        "Bomb:ProximityDistance",
        "Kill:BountyIncreaseForKill",
        "Misc:BounceFactor",
        "Radar:MapZoomFactor",
        "Kill:MaxBonus",
        "Kill:MaxPenalty",
        "Kill:RewardBase",
        "Repel:RepelTime",
        "Repel:RepelDistance",
        "Misc:TickerDelay",
        "Flag:FlaggerOnRadar",
        "Flag:FlaggerKillMultiplier",
        "Prize:PrizeFactor",
        "Prize:PrizeDelay",
        "Prize:MinimumVirtual",
        "Prize:UpgradeVirtual",
        "Prize:PrizeMaxExist",
        "Prize:PrizeMinExist",
        "Prize:PrizeNegativeFactor",
        "Door:DoorDelay",
        "Toggle:AntiWarpPixels",
        "Door:DoorMode",
        "Flag:FlagBlankDelay",
        "Flag:NoDataFlagDropDelay",
        "Prize:MultiPrizeCount",
        "Brick:BrickTime",
        "Misc:WarpRadiusLimit",
        "Bomb:EBombShutdownTime",
        "Bomb:EBombDamagePercent",
        "Radar:RadarNeutralSize",
        "Misc:WarpPointDelay",
        "Misc:NearDeathLevel",
        "Bomb:BBombDamagePercent",
        "Shrapnel:ShrapnelDamagePercent",
        "Latency:ClientSlowPacketTime",
        "Flag:FlagDropResetReward",
        "Flag:FlaggerFireCostPercent",
        "Flag:FlaggerDamagePercent",
        "Flag:FlaggerBombFireDelay",
        "Soccer:PassDelay",
        "Soccer:BallBlankDelay",
        "Latency:S2CNoDataKickoutDelay",
        "Flag:FlaggerThrustAdjustment",
        "Flag:FlaggerSpeedAdjustment",
        "Latency:ClientSlowPacketSampleSize",
        "Unused:Unused5",
        "Unused:Unused4",
        "Unused:Unused3",
        "Unused:Unused2",
        "Unused:Unused1",
    ];

    /// Zone-wide 8-bit settings.
    pub const BYTE_NAMES: [&str; 32] = [
        "Shrapnel:Random",
        "Soccer:BallBounce",
        "Soccer:AllowBombs",
        "Soccer:AllowGuns",
        "Soccer:Mode",
        "Team:MaxPerTeam",
        "Team:MaxPerPrivateTeam",
        "Mine:TeamMaxMines",
        "Wormhole:GravityBombs",
        "Bomb:BombSafety",
        "Chat:MessageReliable",
        "Prize:TakePrizeReliable",
        "Message:AllowAudioMessages",
        "Prize:PrizeHideCount",
        "Misc:ExtraPositionData",
        "Misc:SlowFrameCheck",
        "Flag:CarryFlags",
        "Misc:AllowSavedShips",
        "Radar:RadarMode",
        "Misc:VictoryMusic",
        "Flag:FlaggerGunUpgrade",
        "Flag:FlaggerBombUpgrade",
        "Soccer:UseFlagger",
        "Soccer:BallLocation",
        "Misc:AntiWarpSettleDelay",
        "Unused:Unused7",
        "Unused:Unused6",
        "Unused:Unused5",
        "Unused:Unused4",
        "Unused:Unused3",
        "Unused:Unused2",
        "Unused:Unused1",
    ];

    /// Prize weights (prizes 1..=28), all in section `PrizeWeight`.
    pub const PRIZEWEIGHT_NAMES: [&str; 28] = [
        "PrizeWeight:QuickCharge",
        "PrizeWeight:Energy",
        "PrizeWeight:Rotation",
        "PrizeWeight:Stealth",
        "PrizeWeight:Cloak",
        "PrizeWeight:XRadar",
        "PrizeWeight:Warp",
        "PrizeWeight:Gun",
        "PrizeWeight:Bomb",
        "PrizeWeight:BouncingBullets",
        "PrizeWeight:Thruster",
        "PrizeWeight:TopSpeed",
        "PrizeWeight:Recharge",
        "PrizeWeight:Glue",
        "PrizeWeight:MultiFire",
        "PrizeWeight:Proximity",
        "PrizeWeight:AllWeapons",
        "PrizeWeight:Shields",
        "PrizeWeight:Shrapnel",
        "PrizeWeight:AntiWarp",
        "PrizeWeight:Repel",
        "PrizeWeight:Burst",
        "PrizeWeight:Decoy",
        "PrizeWeight:Thor",
        "PrizeWeight:MultiPrize",
        "PrizeWeight:Brick",
        "PrizeWeight:Rocket",
        "PrizeWeight:Portal",
    ];

    /// Per-ship 32-bit settings.
    pub const SHIP_LONG_NAMES: [&str; 2] = ["SuperTime", "ShieldsTime"];

    /// Per-ship 16-bit settings. Index 10 is the misc bitfield.
    pub const SHIP_SHORT_NAMES: [&str; 49] = [
        "Gravity",
        "GravityTopSpeed",
        "BulletFireEnergy",
        "MultiFireEnergy",
        "BombFireEnergy",
        "BombFireEnergyUpgrade",
        "LandmineFireEnergy",
        "LandmineFireEnergyUpgrade",
        "BulletSpeed",
        "BombSpeed",
        "___MiscBitfield___",
        "MultiFireAngle",
        "CloakEnergy",
        "StealthEnergy",
        "AntiWarpEnergy",
        "XRadarEnergy",
        "MaximumRotation",
        "MaximumThrust",
        "MaximumSpeed",
        "MaximumRecharge",
        "MaximumEnergy",
        "InitialRotation",
        "InitialThrust",
        "InitialSpeed",
        "InitialRecharge",
        "InitialEnergy",
        "UpgradeRotation",
        "UpgradeThrust",
        "UpgradeSpeed",
        "UpgradeRecharge",
        "UpgradeEnergy",
        "AfterburnerEnergy",
        "BombThrust",
        "BurstSpeed",
        "TurretThrustPenalty",
        "TurretSpeedPenalty",
        "BulletFireDelay",
        "MultiFireDelay",
        "BombFireDelay",
        "LandmineFireDelay",
        "RocketTime",
        "InitialBounty",
        "DamageFactor",
        "PrizeShareLimit",
        "AttachBounty",
        "SoccerThrowTime",
        "SoccerBallFriction",
        "SoccerBallProximity",
        "SoccerBallSpeed",
    ];

    /// Per-ship 8-bit settings.
    pub const SHIP_BYTE_NAMES: [&str; 18] = [
        "TurretLimit",
        "BurstShrapnel",
        "MaxMines",
        "RepelMax",
        "BurstMax",
        "DecoyMax",
        "ThorMax",
        "BrickMax",
        "RocketMax",
        "PortalMax",
        "InitialRepel",
        "InitialBurst",
        "InitialBrick",
        "InitialRocket",
        "InitialThor",
        "InitialDecoy",
        "InitialPortal",
        "BombBounceCount",
    ];

    /// Ship weapon bitfield: `(name, bit offset, bit length)` in the 32-bit word.
    pub const SHIP_WEAPON_BITS: [(&str, u32, u32); 14] = [
        ("ShrapnelMax", 0, 5),
        ("ShrapnelRate", 5, 5),
        ("CloakStatus", 10, 2),
        ("StealthStatus", 12, 2),
        ("XRadarStatus", 14, 2),
        ("AntiWarpStatus", 16, 2),
        ("InitialGuns", 18, 2),
        ("MaxGuns", 20, 2),
        ("InitialBombs", 22, 2),
        ("MaxBombs", 24, 2),
        ("DoubleBarrel", 26, 1),
        ("EmpBomb", 27, 1),
        ("SeeMines", 28, 1),
        ("Unused1", 29, 3),
    ];

    /// Ship misc bitfield (ship short setting 10): `(name, bit offset, bit length)`.
    pub const SHIP_MISC_BITS: [(&str, u32, u32); 3] = [
        ("SeeBombLevel", 0, 2),
        ("DisableFastShooting", 2, 1),
        ("Radius", 3, 8),
    ];
}

use names::*;

// byte offsets in the packet (see ASSS packets/clientset.h)
const SHIPS: usize = 4;
const SHIP_SIZE: usize = 144;
const SHIP_LONGS: usize = 0;
const SHIP_SHORTS: usize = 8;
const SHIP_BYTES: usize = 106;
const SHIP_WEAPONS: usize = 124;
const SHIP_MISC: usize = SHIP_SHORTS + 2 * 10;
const LONGS: usize = SHIPS + 8 * SHIP_SIZE;
const SPAWNS: usize = LONGS + 4 * 20;
const SHORTS: usize = SPAWNS + 4 * 4;
const BYTES: usize = SHORTS + 2 * 58;
const PRIZEWEIGHTS: usize = BYTES + 32;
const _: () = assert!(PRIZEWEIGHTS + 28 == SETTINGS_LEN);

/// Indices into [`LONG_NAMES`] that ASSS multiplies by 1000.
const TIMES_1000: [usize; 5] = [0, 1, 10, 11, 16];

/// Header flag bits that are loaded from config: `(section, key, bit, len)`.
const HEADER_LOAD_BITS: [(&str, &str, u32, u32); 8] = [
    ("Bullet", "ExactDamage", 8, 1),
    ("Spectator", "HideFlags", 9, 1),
    ("Spectator", "NoXRadar", 10, 1),
    ("Misc", "SlowFrameCheck", 11, 3),
    ("Misc", "DisableScreenshot", 14, 1),
    ("Misc", "MaxTimerDrift", 16, 3),
    ("Soccer", "DisableWallPass", 19, 1),
    ("Soccer", "DisableBallKilling", 20, 1),
];

/// Header flag bits addressable by override key (note the different names).
const HEADER_OVERRIDE_BITS: [(&str, u32, u32); 8] = [
    ("Bullet:ExactDamage", 8, 1),
    ("Spectator:HideFlags", 9, 1),
    ("Spectator:NoXRadar", 10, 1),
    ("Misc:SlowFrameCheck", 11, 3),
    ("Misc:DisableScreenshot", 14, 1),
    ("Misc:MaxTimerDrift", 16, 3),
    ("Misc:DisableBallThroughWalls", 19, 1),
    ("Misc:DisableBallKilling", 20, 1),
];

/// The 1428-byte settings packet.
#[derive(Clone, PartialEq, Eq)]
pub struct SettingsBlock {
    bytes: [u8; SETTINGS_LEN],
}

impl std::fmt::Debug for SettingsBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsBlock")
            .field("type", &self.bytes[0])
            .finish_non_exhaustive()
    }
}

impl Default for SettingsBlock {
    fn default() -> Self {
        Self {
            bytes: [0; SETTINGS_LEN],
        }
    }
}

impl SettingsBlock {
    /// The packet bytes, ready to send reliably.
    pub fn as_bytes(&self) -> &[u8; SETTINGS_LEN] {
        &self.bytes
    }

    /// Wraps raw packet bytes.
    pub fn from_bytes(bytes: [u8; SETTINGS_LEN]) -> Self {
        Self { bytes }
    }

    /// Writes the low `len` bits of `value` at little-endian bit offset `bit`.
    fn set_bits(&mut self, bit: u32, len: u32, value: u32) {
        for i in 0..len {
            let b = (bit + i) as usize;
            let mask = 1u8 << (b % 8);
            if (value >> i) & 1 == 1 {
                self.bytes[b / 8] |= mask;
            } else {
                self.bytes[b / 8] &= !mask;
            }
        }
    }

    /// Reads `len` bits at little-endian bit offset `bit` (unsigned).
    pub fn get_bits(&self, bit: u32, len: u32) -> u32 {
        (0..len.min(32)).fold(0, |acc, i| {
            let b = (bit + i) as usize;
            let v = self.bytes.get(b / 8).map_or(0, |x| (x >> (b % 8)) & 1);
            acc | (u32::from(v) << i)
        })
    }

    /// Reads the field an override key addresses (unsigned).
    pub fn get(&self, key: OverrideKey) -> u32 {
        self.get_bits(key.offset(), key.len())
    }

    /// ASSS `GetChecksum`: sum of every little-endian 32-bit word xor `key`.
    pub fn checksum(&self, key: u32) -> u32 {
        self.bytes.as_chunks::<4>().0.iter().fold(0u32, |sum, w| {
            sum.wrapping_add(u32::from_le_bytes(*w) ^ key)
        })
    }

    /// Applies arena and then player overrides (ASSS `do_mask`).
    pub fn with_overrides(&self, arena: &Overrides, player: Option<&Overrides>) -> SettingsBlock {
        let mut out = self.clone();
        arena.apply(&mut out.bytes);
        if let Some(p) = player {
            p.apply(&mut out.bytes);
        }
        out
    }
}

/// Partial sums of prize weights, for weighted random prizes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrizeWeights {
    sums: [u16; 28],
}

impl PrizeWeights {
    /// Running totals for the 28 `PrizeWeight` values (prize 1 first).
    pub fn from_weights(weights: &[u8; 28]) -> Self {
        let mut sums = [0u16; 28];
        let mut total = 0u16;
        for (s, &w) in sums.iter_mut().zip(weights) {
            total = total.wrapping_add(u16::from(w));
            *s = total;
        }
        Self { sums }
    }

    /// Total weight.
    pub fn total(&self) -> u16 {
        self.sums[27]
    }

    /// The partial sums (prize `i + 1` covers `sums[i-1]..sums[i]`).
    pub fn sums(&self) -> &[u16; 28] {
        &self.sums
    }

    /// ASSS `GetRandomPrize`: `rand_below(n)` must return a value in `0..n`.
    /// Returns a prize number in `1..=28`, or 0 if all weights are zero.
    pub fn random_prize(&self, rand_below: impl FnOnce(u32) -> u32) -> u8 {
        let max = self.sums[27];
        if max == 0 {
            return 0;
        }
        let r = rand_below(u32::from(max)).min(u32::from(max) - 1);
        let (mut i, mut j) = (0usize, 27usize);
        while r >= u32::from(self.sums[i]) {
            let m = (i + j) / 2;
            if r < u32::from(self.sums[m]) {
                j = m;
            } else {
                i = m + 1;
            }
        }
        (i + 1) as u8
    }
}

/// Arena settings as built from config: the packet and the prize weight sums.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArenaSettings {
    /// The packet before overrides.
    pub block: SettingsBlock,
    /// Prize weight partial sums.
    pub prize_weights: PrizeWeights,
}

/// ASSS `load_settings`.
pub fn load(cfg: &ConfigFile) -> ArenaSettings {
    let mut s = SettingsBlock::default();
    s.bytes[0] = S2C_SETTINGS;
    for (sec, key, bit, len) in HEADER_LOAD_BITS {
        s.set_bits(bit, len, cfg.get_int(sec, key, 0) as u32);
    }

    for (i, ship) in SHIP_NAMES.iter().enumerate() {
        let base = SHIPS + i * SHIP_SIZE;
        for (j, name) in SHIP_LONG_NAMES.iter().enumerate() {
            let off = base + SHIP_LONGS + 4 * j;
            s.set_bits(8 * off as u32, 32, cfg.get_int(ship, name, 0) as u32);
        }
        for (j, name) in SHIP_SHORT_NAMES.iter().enumerate() {
            let off = base + SHIP_SHORTS + 2 * j;
            s.set_bits(8 * off as u32, 16, cfg.get_int(ship, name, 0) as u32);
        }
        for (j, name) in SHIP_BYTE_NAMES.iter().enumerate() {
            let off = base + SHIP_BYTES + j;
            s.set_bits(8 * off as u32, 8, cfg.get_int(ship, name, 0) as u32);
        }
        let wbase = 8 * (base + SHIP_WEAPONS) as u32;
        for (name, bit, len) in SHIP_WEAPON_BITS {
            s.set_bits(wbase + bit, len, cfg.get_int(ship, name, 0) as u32);
        }
        // the misc bitfield replaces all 16 bits of short setting 10
        let mbase = 8 * (base + SHIP_MISC) as u32;
        s.set_bits(mbase, 16, 0);
        for (name, bit, len) in SHIP_MISC_BITS {
            s.set_bits(mbase + bit, len, cfg.get_int(ship, name, 0) as u32);
        }
    }

    for t in 0..4u32 {
        let base = 8 * (SPAWNS as u32 + 4 * t);
        s.set_bits(
            base,
            10,
            cfg.get_int("Spawn", &format!("Team{t}-X"), 0) as u32,
        );
        s.set_bits(
            base + 10,
            10,
            cfg.get_int("Spawn", &format!("Team{t}-Y"), 0) as u32,
        );
        s.set_bits(
            base + 20,
            9,
            cfg.get_int("Spawn", &format!("Team{t}-Radius"), 0) as u32,
        );
    }

    for (i, name) in LONG_NAMES.iter().enumerate() {
        let mut v = cfg.get_int_raw(name, 0);
        if TIMES_1000.contains(&i) {
            v = v.wrapping_mul(1000);
        }
        s.set_bits(8 * (LONGS + 4 * i) as u32, 32, v as u32);
    }
    for (i, name) in SHORT_NAMES.iter().enumerate() {
        s.set_bits(
            8 * (SHORTS + 2 * i) as u32,
            16,
            cfg.get_int_raw(name, 0) as u32,
        );
    }
    for (i, name) in BYTE_NAMES.iter().enumerate() {
        s.set_bits(8 * (BYTES + i) as u32, 8, cfg.get_int_raw(name, 0) as u32);
    }
    let mut pw = PrizeWeights::default();
    let mut total = 0u16;
    for (i, name) in PRIZEWEIGHT_NAMES.iter().enumerate() {
        let w = cfg.get_int_raw(name, 0) as u8;
        s.bytes[PRIZEWEIGHTS + i] = w;
        total = total.wrapping_add(u16::from(w));
        pw.sums[i] = total;
    }
    ArenaSettings {
        block: s,
        prize_weights: pw,
    }
}

/// An ASSS override key: bit offset in the low 16 bits, bit length in the high 16.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OverrideKey(pub u32);

impl OverrideKey {
    fn new(bit: usize, len: u32) -> Self {
        Self(bit as u32 | (len << 16))
    }

    /// Bit offset of the field.
    pub fn offset(self) -> u32 {
        self.0 & 0xffff
    }

    /// Bit length of the field.
    pub fn len(self) -> u32 {
        self.0 >> 16
    }

    /// True for a zero-length key (never returned by [`override_key`]).
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// ASSS `GetOverrideKey`: maps `section`/`key` to the field it sets.
    /// Returns `None` for names that aren't client settings.
    pub fn lookup(section: &str, key: &str) -> Option<Self> {
        override_key(section, key)
    }
}

/// ASSS `GetOverrideKey`.
pub fn override_key(section: &str, key: &str) -> Option<OverrideKey> {
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    if eq(section, "PrizeWeight") {
        return PRIZEWEIGHT_NAMES
            .iter()
            .position(|n| eq(&n["PrizeWeight:".len()..], key))
            .map(|i| OverrideKey::new(8 * (PRIZEWEIGHTS + i), 8));
    }
    if let Some(ship) = SHIP_NAMES.iter().position(|n| eq(n, section)) {
        let base = SHIPS + ship * SHIP_SIZE;
        if let Some(j) = SHIP_LONG_NAMES.iter().position(|n| eq(n, key)) {
            return Some(OverrideKey::new(8 * (base + SHIP_LONGS + 4 * j), 32));
        }
        if let Some(j) = SHIP_SHORT_NAMES.iter().position(|n| eq(n, key)) {
            return Some(OverrideKey::new(8 * (base + SHIP_SHORTS + 2 * j), 16));
        }
        if let Some(j) = SHIP_BYTE_NAMES.iter().position(|n| eq(n, key)) {
            return Some(OverrideKey::new(8 * (base + SHIP_BYTES + j), 8));
        }
        for (name, bit, len) in SHIP_WEAPON_BITS {
            if eq(name, key) {
                return Some(OverrideKey::new(
                    8 * (base + SHIP_WEAPONS) + bit as usize,
                    len,
                ));
            }
        }
        for (name, bit, len) in SHIP_MISC_BITS {
            if eq(name, key) {
                return Some(OverrideKey::new(8 * (base + SHIP_MISC) + bit as usize, len));
            }
        }
        return None;
    }
    if eq(section, "Spawn") {
        for t in 0..4usize {
            let base = 8 * (SPAWNS + 4 * t);
            if eq(&format!("Team{t}-X"), key) {
                return Some(OverrideKey::new(base, 10));
            }
            if eq(&format!("Team{t}-Y"), key) {
                return Some(OverrideKey::new(base + 10, 10));
            }
            if eq(&format!("Team{t}-Radius"), key) {
                return Some(OverrideKey::new(base + 20, 9));
            }
        }
        return None;
    }
    let full = format!("{section}:{key}");
    if let Some(i) = LONG_NAMES.iter().position(|n| eq(n, &full)) {
        return Some(OverrideKey::new(8 * (LONGS + 4 * i), 32));
    }
    if let Some(i) = SHORT_NAMES.iter().position(|n| eq(n, &full)) {
        return Some(OverrideKey::new(8 * (SHORTS + 2 * i), 16));
    }
    if let Some(i) = BYTE_NAMES.iter().position(|n| eq(n, &full)) {
        return Some(OverrideKey::new(8 * (BYTES + i), 8));
    }
    HEADER_OVERRIDE_BITS
        .iter()
        .find(|(n, _, _)| eq(n, &full))
        .map(|&(_, bit, len)| OverrideKey::new(bit as usize, len))
}

/// A set of overridden bits (ASSS `overridedata`).
#[derive(Clone, PartialEq, Eq)]
pub struct Overrides {
    bits: [u8; SETTINGS_LEN],
    mask: [u8; SETTINGS_LEN],
}

impl Default for Overrides {
    fn default() -> Self {
        Self {
            bits: [0; SETTINGS_LEN],
            mask: [0; SETTINGS_LEN],
        }
    }
}

impl std::fmt::Debug for Overrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.mask.iter().map(|b| b.count_ones()).sum::<u32>();
        f.debug_struct("Overrides")
            .field("overridden_bits", &n)
            .finish()
    }
}

impl Overrides {
    /// Returns false (and changes nothing) for keys ASSS rejects: the type
    /// byte, or fields that straddle a 32-bit word.
    fn valid(key: OverrideKey) -> bool {
        let (off, len) = (key.offset(), key.len());
        off >= 8 && len <= 32 && (off & 31) + len <= 32 && (off + len) as usize <= 8 * SETTINGS_LEN
    }

    /// Overrides a field with `value` (truncated to the field width).
    pub fn set(&mut self, key: OverrideKey, value: i32) -> bool {
        if !Self::valid(key) {
            return false;
        }
        for i in 0..key.len() {
            let b = (key.offset() + i) as usize;
            let m = 1u8 << (b % 8);
            if (value as u32 >> i) & 1 == 1 {
                self.bits[b / 8] |= m;
            } else {
                self.bits[b / 8] &= !m;
            }
            self.mask[b / 8] |= m;
        }
        true
    }

    /// Removes an override.
    pub fn unset(&mut self, key: OverrideKey) -> bool {
        if !Self::valid(key) {
            return false;
        }
        for i in 0..key.len() {
            let b = (key.offset() + i) as usize;
            self.mask[b / 8] &= !(1u8 << (b % 8));
        }
        true
    }

    /// True if nothing is overridden.
    pub fn is_empty(&self) -> bool {
        self.mask.iter().all(|&m| m == 0)
    }

    fn apply(&self, bytes: &mut [u8; SETTINGS_LEN]) {
        for ((d, &o), &m) in bytes.iter_mut().zip(&self.bits).zip(&self.mask) {
            *d = (*d & !m) | (o & m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_type_byte() {
        let s = load(&ConfigFile::default());
        assert_eq!(s.block.as_bytes()[0], S2C_SETTINGS);
        assert!(s.block.as_bytes()[1..].iter().all(|&b| b == 0));
        assert_eq!(s.prize_weights.random_prize(|_| 0), 0);
    }

    #[test]
    fn fields_land_where_asss_puts_them() {
        let cfg = ConfigFile::from_text(
            "[Warbird]\nSuperTime=6000\nGravity=-2\nTurretLimit=300\nShrapnelMax=33\n\
             Radius=14\nSeeBombLevel=3\n\
             [Shark]\nBombBounceCount=7\n\
             [Bullet]\nBulletDamageLevel=200\nExactDamage=1\n\
             [Spawn]\nTeam1-Y=1023\nTeam1-Radius=600\n\
             [Misc]\nBounceFactor=16\nMaxTimerDrift=7\n\
             [Soccer]\nDisableWallPass=1\n\
             [PrizeWeight]\nQuickCharge=3\nPortal=2\n",
        );
        let s = load(&cfg);
        let b = s.block.as_bytes();
        assert_eq!(&b[4..8], &6000i32.to_le_bytes());
        assert_eq!(&b[12..14], &(-2i16).to_le_bytes());
        assert_eq!(b[4 + 106], 300u32 as u8);
        assert_eq!(b[4 + 7 * 144 + 106 + 17], 7);
        // ShrapnelMax is 5 bits: 33 -> 1
        assert_eq!(
            u32::from_le_bytes([b[128], b[129], b[130], b[131]]) & 0x1f,
            1
        );
        // misc bitfield: SeeBombLevel=3 | Radius=14<<3
        assert_eq!(u16::from_le_bytes([b[32], b[33]]), 3 | (14 << 3));
        // BulletDamageLevel * 1000
        assert_eq!(&b[1156..1160], &200_000i32.to_le_bytes());
        // header: type | ExactDamage | MaxTimerDrift=7 | DisableBallThroughWalls
        assert_eq!(
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            0x0f | 1 << 8 | 7 << 16 | 1 << 19
        );
        // spawn team 1: y=1023, r=600 truncated to 9 bits
        let sp = u32::from_le_bytes([b[1240], b[1241], b[1242], b[1243]]);
        assert_eq!((sp >> 10) & 0x3ff, 1023);
        assert_eq!((sp >> 20) & 0x1ff, 600 & 0x1ff);
        assert_eq!(&b[1252 + 20..1252 + 22], &16i16.to_le_bytes());
        assert_eq!(b[1400], 3);
        assert_eq!(b[1427], 2);
        assert_eq!(s.prize_weights.total(), 5);
        assert_eq!(s.prize_weights.random_prize(|_| 0), 1);
        assert_eq!(s.prize_weights.random_prize(|_| 2), 1);
        assert_eq!(s.prize_weights.random_prize(|_| 3), 28);
        assert_eq!(s.prize_weights.random_prize(|n| n - 1), 28);
    }

    #[test]
    fn overrides_mask_and_layer() {
        let base = load(&ConfigFile::from_text("[Warbird]\nInitialSpeed=2000\n")).block;
        let k = override_key("warbird", "initialspeed").expect("key");
        assert_eq!(base.get(k), 2000);
        let mut arena = Overrides::default();
        let mut player = Overrides::default();
        assert!(arena.set(k, 1500));
        assert_eq!(base.with_overrides(&arena, None).get(k), 1500);
        assert!(player.set(k, -1));
        assert_eq!(base.with_overrides(&arena, Some(&player)).get(k), 0xffff);
        assert!(arena.unset(k));
        assert!(player.unset(k));
        assert_eq!(base.with_overrides(&arena, Some(&player)), base);
        // the type byte can't be overridden
        assert!(!arena.set(OverrideKey(8 << 16), 0));
        assert!(override_key("Warbird", "NoSuchSetting").is_none());
        assert!(override_key("NoSection", "X").is_none());
        let drift = override_key("Misc", "DisableBallThroughWalls").expect("bit");
        assert_eq!((drift.offset(), drift.len()), (19, 1));
    }
}
