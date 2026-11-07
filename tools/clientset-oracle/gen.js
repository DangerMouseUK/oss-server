// Generates a synthetic arena.conf exercising every client setting, plus the
// flat key/value file and override-key name list for the C oracle.
const fs = require('fs');
const src = fs.readFileSync(process.argv[2], 'utf8'); // clientset.rs (names source)
const arr = (name) => {
  const start = src.indexOf(`pub const ${name}:`); const body = src.slice(src.indexOf("[", src.indexOf("=", start)) + 1, src.indexOf("];", start)); const m = [null, body];
  return [...m[1].matchAll(/"([^"]+)"/g)].map(x => x[1]);
};
let seed = 0x5eed1234;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 8) % n; };
const value = () => {
  switch (rnd(12)) {
    case 0: return String(rnd(1000));
    case 1: return String(-rnd(40000));
    case 2: return String(rnd(2147483647));
    case 3: return rnd(2) ? '99999999999' : '-99999999999';
    case 4: return '0x' + rnd(0x7fffffff).toString(16)[rnd(2) ? 'toUpperCase' : 'toLowerCase']();
    case 5: return '0' + rnd(0o777777).toString(8);
    case 6: return ['yes', 'Yes', 'no', 'y', 'N'][rnd(5)];
    case 7: return ['abc', '$SEE_TEAM', '-', '0x'][rnd(4)];
    case 8: return null;
    case 9: return rnd(3000) + 'abc';
    case 10: return String(rnd(4));
    default: return String(rnd(70000) - 5000);
  }
};
const sections = new Map();
const flat = [];
const put = (sec, key) => {
  const v = value(); if (v === null) return;
  if (!sections.has(sec)) sections.set(sec, []);
  sections.get(sec).push(`${key} = ${v}`);
  flat.push(`${sec}:${key}\t${v}`);
};
const ships = arr('SHIP_NAMES');
const shipKeys = [...arr('SHIP_LONG_NAMES'), ...arr('SHIP_SHORT_NAMES'), ...arr('SHIP_BYTE_NAMES'),
  ...[...src.match(/SHIP_WEAPON_BITS[\s\S]*?\];/)[0].matchAll(/\("(\w+)"/g)].map(x => x[1]),
  'SeeBombLevel', 'DisableFastShooting', 'Radius'];
for (const s of ships) for (const k of shipKeys) put(s, k);
const header = [['Bullet','ExactDamage'],['Spectator','HideFlags'],['Spectator','NoXRadar'],['Misc','SlowFrameCheck'],
  ['Misc','DisableScreenshot'],['Misc','MaxTimerDrift'],['Soccer','DisableWallPass'],['Soccer','DisableBallKilling']];
const full = [...arr('LONG_NAMES'), ...arr('SHORT_NAMES'), ...arr('BYTE_NAMES'), ...arr('PRIZEWEIGHT_NAMES')];
for (let t = 0; t < 4; t++) for (const k of ['X', 'Y', 'Radius']) put('Spawn', `Team${t}-${k}`);
for (const f of full) { const [s, k] = f.split(':'); put(s, k); }
for (const [s, k] of header) put(s, k);
let conf = '; Synthetic arena config covering every client setting (generated).\n';
for (const [s, lines] of sections) conf += `\n[${s}]\n` + lines.join('\n') + '\n';
fs.writeFileSync('synthetic.conf', conf);
fs.writeFileSync('flat.txt', flat.join('\n') + '\n');
const names = [];
for (const s of ships) for (const k of shipKeys) names.push(`${s}:${k}`);
for (let t = 0; t < 4; t++) for (const k of ['X', 'Y', 'Radius']) names.push(`Spawn:Team${t}-${k}`);
names.push(...full, 'Bullet:ExactDamage', 'Spectator:HideFlags', 'Spectator:NoXRadar', 'Misc:DisableScreenshot',
  'Misc:MaxTimerDrift', 'Misc:DisableBallThroughWalls', 'Misc:DisableBallKilling', 'Soccer:DisableWallPass',
  'WARBIRD:initialspeed', 'prizeweight:PORTAL', 'spawn:team3-radius', 'Nope:Nope', 'Warbird:Nope', 'Spawn:Team4-X',
  'PrizeWeight:Nope', 'Warbird:___MiscBitfield___');
fs.writeFileSync('names.txt', names.join('\n') + '\n');
console.log(flat.length, 'values,', names.length, 'names');
