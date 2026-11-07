/* Oracle: ASSS 1.4.4 clientset.c load_settings/GetOverrideKey/GetChecksum,
 * compiled verbatim against packets/clientset.h and clientset.def.
 * Input: flat file of "Section:Key<TAB>value" lines. GetInt = ASSS GetInt. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stddef.h>
typedef unsigned int u32; typedef unsigned short u16; typedef unsigned char u8, byte;
typedef int i32; typedef short i16; typedef signed char i8;
#define local static
#define MAXSECTIONLEN 64
#define MAXKEYLEN 64
#define S2C_SETTINGS 0x0F
#ifdef _MSC_VER
#define strcasecmp _stricmp
#endif
#include "packets/clientset.h"
#include "core/clientset.def"
#define COUNT(x) (sizeof(x)/sizeof(x[0]))
typedef u32 override_key_t;

static char *keys[4096], *vals[4096]; static int nkv;
static const char *GetStr(const char *sec, const char *key) {
	char k[256]; int i;
	if (sec && key) snprintf(k, sizeof k, "%s:%s", sec, key); else snprintf(k, sizeof k, "%s", sec ? sec : key);
	for (i = nkv - 1; i >= 0; i--) if (!_stricmp(keys[i], k)) return vals[i];
	return NULL;
}
static int GetInt(void *conf, const char *sec, const char *key, int def) {
	char *next; const char *str = GetStr(sec, key); int ret;
	if (!str) return def;
	ret = strtol(str, &next, 0);
	return str != next ? ret : str[0] == 'y' || str[0] == 'Y';
}
struct { int (*GetInt)(void*, const char*, const char*, int); } cfgs = { GetInt }, *cfg = &cfgs;
typedef void *ConfigHandle;
typedef struct adata { struct ClientSettings cs; unsigned short pwps[32]; } adata;
