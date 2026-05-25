/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _VTL_PERSONALITY_H_
#define _VTL_PERSONALITY_H_

struct vtl_personality_desc {
	const char *name;
	const char *vendor;          /* 8 bytes */
	const char *product_changer; /* 16 bytes */
	const char *product_tape;    /* 16 bytes */
	const char *revision;        /* 4 bytes INQUIRY bytes 32-35 */
};

#define VTL_PERSONALITY_VTL 0
#define VTL_PERSONALITY_IBM 1
#define VTL_PERSONALITY_STK 2
#define VTL_PERSONALITY_HP  3

const struct vtl_personality_desc *vtl_personality_lookup(int id);
int vtl_personality_resolve_name(const char *name);
int vtl_personality_active_id(void);
void vtl_personality_set_active(int id);

#endif /* _VTL_PERSONALITY_H_ */
