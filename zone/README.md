# Bundled default zone

`oss-server` runs this zone when started without `--zone`. It is compiled
into the binary, so no files are needed at run time; `oss-server --init-zone
<dir>` writes a copy to disk to customise, and `oss-server --zone <dir>` runs
any ASSS-style zone directory.

Contents come from the ASSS 1.4.4 distribution (GPL-2.0-or-later, like this
repository): `conf/groupdef.conf`, `conf/groupdef.dir/`, `conf/svs/`,
`arenas/(default)/`, `arenas/(public)/`, `arenas/scf/` (with `scf.lvl`) and
`maps/smallmap.lvl` are unchanged except where a file says otherwise.
`conf/modules.conf`, `conf/global.conf` and `arenas/(default)/arena.conf` are
trimmed versions of ASSS's files (each explains what changed and why);
`conf/staff.conf`, `conf/passwd.conf` and `news.txt` are new.

Changes made to the zone while running from the embedded copy (e.g.
`?passwd`) last only until the server stops; use `--init-zone` and `--zone`
for a persistent zone.
