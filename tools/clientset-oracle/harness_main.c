static char *trimnl(char *s) { char *t; if ((t = strchr(s, '\n'))) *t = 0; if ((t = strchr(s, '\r'))) *t = 0; return s; }
int main(int argc, char **argv) {
	static char line[8192]; static adata ad; FILE *f; int i; u32 csum = 0, key;
	if (argc < 4) return 2;
	f = fopen(argv[1], "rb");
	while (fgets(line, sizeof line, f)) {
		char *tab = strchr(trimnl(line), '\t'); if (!tab) continue; *tab = 0;
		keys[nkv] = _strdup(line); vals[nkv] = _strdup(tab + 1); nkv++;
	}
	fclose(f);
	if (sizeof(struct ClientSettings) != 1428) { printf("bad size %d\n", (int)sizeof(struct ClientSettings)); return 1; }
	load_settings(&ad, NULL);
	printf("settings ");
	for (i = 0; i < 1428; i++) printf("%02x", ((u8*)&ad.cs)[i]);
	printf("\n");
	key = (u32)strtoul(argv[3], NULL, 0);
	for (i = 0; i < 1428 / 4; i++) csum += ((u32*)&ad.cs)[i] ^ key;
	printf("checksum %08x %08x\n", key, csum);
	printf("pwps"); for (i = 0; i < 28; i++) printf(" %u", ad.pwps[i]); printf("\n");
	f = fopen(argv[2], "rb");
	while (fgets(line, sizeof line, f)) {
		char *c = strchr(trimnl(line), ':'); if (!c) continue; *c = 0;
		printf("key %s:%s %08x\n", line, c + 1, GetOverrideKey(line, c + 1));
	}
	fclose(f);
	return 0;
}
