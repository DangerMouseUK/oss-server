# clientset oracle

Regenerates `crates/oss-server/tests/fixtures/clientset/` from ASSS 1.4.4's own
settings code, so the golden test compares `oss-server` against ASSS rather than
against our reading of it.

The harness compiles ASSS's `load_settings` and `GetOverrideKey` unchanged,
against ASSS's `packets/clientset.h` and `core/clientset.def`, with a minimal
`GetInt` that has ASSS's semantics (`strtol(s, &end, 0)`, else `y`/`Y` → 1).
Build it for a target whose `long` is 32 bits (ASSS's target), e.g. MSVC x64 or
`gcc -m32`.

Steps, with `ASSS=<asss-1.4.4>/src` (an ASSS source tree; not in this repo):

```bash
node gen.js ../../crates/oss-server/src/clientset.rs   # writes synthetic.conf, flat.txt, names.txt
{ cat harness_head.c
  sed -n '/^local void load_settings/,/^}/p' "$ASSS/core/clientset.c"
  sed -n '/^local override_key_t GetOverrideKey/,/^}/p' "$ASSS/core/clientset.c"
  cat harness_main.c; } > oracle.c
gcc -m32 -D_stricmp=strcasecmp -D_strdup=strdup -I "$ASSS" -I "$ASSS/include" oracle.c -o oracle
# MSVC: cl /D_CRT_SECURE_NO_WARNINGS /I %ASSS% /I %ASSS%\include oracle.c
./oracle flat.txt names.txt 0x9e3779b9 > golden.txt
cp synthetic.conf golden.txt ../../crates/oss-server/tests/fixtures/clientset/
```

`synthetic.conf` is plain `[Section]`/`Key = value` text with no leading or
trailing blanks in values, so ASSS's config parser yields exactly the values in
`flat.txt`; the config parser itself is tested separately.
