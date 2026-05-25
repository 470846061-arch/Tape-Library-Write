// SPDX-License-Identifier: GPL-2.0
/*
 * SCSI INQUIRY vendor/product strings (mhVTL-compatible personalities).
 * Module parameter personality=ibm|stk|hp|vtl selects the active table at load.
 */
#include "../include/vtl_personality.h"
#include "../include/vtl.h"
#include <linux/kernel.h>
#include <linux/string.h>

static int vtl_active_personality = VTL_PERSONALITY_VTL;

static const struct vtl_personality_desc vtl_personality_table[] = {
	[VTL_PERSONALITY_VTL] = {
		.name = "vtl",
		.vendor = VTL_VENDOR_ID,
		.product_changer = VTL_PRODUCT_CHANGER,
		.product_tape = VTL_PRODUCT_TAPE,
		.revision = VTL_REVISION,
	},
	[VTL_PERSONALITY_IBM] = {
		.name = "ibm",
		.vendor = "IBM     ",
		.product_changer = "03584L32 A00    ",
		.product_tape = "ULT3580-TD8     ",
		.revision = "0106",
	},
	[VTL_PERSONALITY_STK] = {
		.name = "stk",
		.vendor = "STK     ",
		.product_changer = "L700            ",
		.product_tape = "T10000B         ",
		.revision = "0105",
	},
	[VTL_PERSONALITY_HP] = {
		.name = "hp",
		.vendor = "HP      ",
		.product_changer = "MSL6480         ",
		.product_tape = "Ultrium 5-SCSI  ",
		.revision = "0105",
	},
};

int vtl_personality_active_id(void)
{
	return vtl_active_personality;
}

void vtl_personality_set_active(int id)
{
	if (id < 0 || id >= (int)ARRAY_SIZE(vtl_personality_table))
		id = VTL_PERSONALITY_VTL;
	vtl_active_personality = id;
}

const struct vtl_personality_desc *vtl_personality_lookup(int id)
{
	if (id < 0 || id >= (int)ARRAY_SIZE(vtl_personality_table))
		id = VTL_PERSONALITY_VTL;
	return &vtl_personality_table[id];
}

static bool vtl_name_eq(const char *a, const char *b)
{
	if (!a || !b)
		return false;
	while (*a && *b) {
		char ca = *a;
		char cb = *b;
		if (ca >= 'A' && ca <= 'Z')
			ca += 'a' - 'A';
		if (cb >= 'A' && cb <= 'Z')
			cb += 'a' - 'A';
		if (ca != cb)
			return false;
		a++;
		b++;
	}
	return *a == *b;
}

int vtl_personality_resolve_name(const char *name)
{
	int i;

	if (!name || !*name)
		return VTL_PERSONALITY_VTL;
	for (i = 0; i < (int)ARRAY_SIZE(vtl_personality_table); i++) {
		if (vtl_name_eq(name, vtl_personality_table[i].name))
			return i;
	}
	/* Legacy aliases used in mhVTL / backup docs */
	if (vtl_name_eq(name, "spectra") || vtl_name_eq(name, "sl") ||
	    vtl_name_eq(name, "l700"))
		return VTL_PERSONALITY_STK;
	if (vtl_name_eq(name, "ts3500") || vtl_name_eq(name, "3584"))
		return VTL_PERSONALITY_IBM;
	if (vtl_name_eq(name, "msl"))
		return VTL_PERSONALITY_HP;
	pr_warn("VTL: unknown personality '%s', using generic VTL\n", name);
	return VTL_PERSONALITY_VTL;
}
