#include "../include/vtl.h"

#include <linux/capability.h>
#include <linux/ioctl.h>
#include <linux/miscdevice.h>
#include <linux/uaccess.h>

#define VTL_IOCTL_CREATE_TAPE _IOW('V', 1, struct vtl_create_req)
#define VTL_IOCTL_LOAD_TAPE _IOW('V', 2, struct vtl_load_req)
#define VTL_IOCTL_UNLOAD_TAPE _IOW('V', 3, struct vtl_unload_req)
#define VTL_IOCTL_STATUS _IOR('V', 4, struct vtl_status)
#define VTL_IOCTL_SLOT_PLACE _IOW('V', 6, struct vtl_slot_place_req)
#define VTL_IOCTL_MOVE_MEDIUM _IOW('V', 7, struct vtl_move_req)

struct vtl_elem_remove_req {
	int instance;
	int element;
};

#define VTL_IOCTL_ELEM_REMOVE _IOW('V', 9, struct vtl_elem_remove_req)

#define VTL_INV_MAX_ITEMS 128

struct vtl_inv_item {
	int element;
	char tape_name[64];
};

struct vtl_inventory_ioctl {
	int instance;
	int num_drives;
	int num_slots;
	int num_mailslots;
	int count;
	int truncated;
	struct vtl_inv_item items[VTL_INV_MAX_ITEMS];
};

#define VTL_IOCTL_GET_INVENTORY _IOWR('V', 8, struct vtl_inventory_ioctl)

struct vtl_set_instances_ioctl {
	char spec[VTL_INST_SPEC_MAX];
};

#define VTL_IOCTL_SET_INSTANCES _IOW('V', 5, struct vtl_set_instances_ioctl)
#define VTL_IOCTL_RESIZE_GEOMETRY _IOW('V', 10, struct vtl_set_instances_ioctl)

struct vtl_create_req {
	char name[64];
	u64 size;
};

struct vtl_load_req {
	int instance;
	int slot;
	int drive;
	char tape_name[64];
	char barcode[16];
};

struct vtl_unload_req {
	int instance;
	int drive;
	int slot;
	char tape_name[64];
};

struct vtl_slot_place_req {
	int instance;
	int slot;
	char tape_name[64];
	char barcode[16];
};

struct vtl_move_req {
	int instance;
	int src;
	int dst;
};

struct vtl_status {
	int num_drives;
	int num_slots;
	int num_mailslots;
};

static long vtl_ioctl_get_inventory(void __user *uarg)
{
	struct vtl_inventory_ioctl *inv;
	int *elements;
	char *names;
	struct vtl_changer *ch;
	int i, ret;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;

	inv = kvmalloc(sizeof(*inv), GFP_KERNEL);
	elements = kvmalloc_array(VTL_INV_MAX_ITEMS, sizeof(*elements), GFP_KERNEL);
	names = kvmalloc_array(VTL_INV_MAX_ITEMS, 64, GFP_KERNEL);
	if (!inv || !elements || !names) {
		kvfree(names);
		kvfree(elements);
		kvfree(inv);
		return -ENOMEM;
	}

	if (copy_from_user(inv, uarg, sizeof(*inv))) {
		ret = -EFAULT;
		goto out;
	}

	ch = vtl_changer_get_instance(inv->instance);
	if (!ch) {
		ret = -ENODEV;
		goto out;
	}

	inv->truncated = 0;
	ret = vtl_changer_collect_inventory(
		ch, &inv->num_drives, &inv->num_slots,
		&inv->num_mailslots, &inv->count, &inv->truncated,
		elements, (char (*)[64])names, VTL_INV_MAX_ITEMS);
	if (ret)
		goto out;

	for (i = 0; i < inv->count && i < VTL_INV_MAX_ITEMS; i++) {
		inv->items[i].element = elements[i];
		strscpy(inv->items[i].tape_name, &names[i * 64],
			sizeof(inv->items[i].tape_name));
	}

	if (copy_to_user(uarg, inv, sizeof(*inv)))
		ret = -EFAULT;
	else
		ret = 0;

out:
	kvfree(names);
	kvfree(elements);
	kvfree(inv);
	return ret;
}

static long vtl_ioctl_create_tape(void __user *uarg)
{
	struct vtl_create_req create_req;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&create_req, uarg, sizeof(create_req)))
		return -EFAULT;
	create_req.name[sizeof(create_req.name) - 1] = '\0';
	return vtl_tape_create(create_req.name, create_req.size);
}

static long vtl_ioctl_load_tape(void __user *uarg)
{
	struct vtl_load_req load_req;
	struct vtl_changer *ch;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&load_req, uarg, sizeof(load_req)))
		return -EFAULT;
	load_req.tape_name[sizeof(load_req.tape_name) - 1] = '\0';
	load_req.barcode[sizeof(load_req.barcode) - 1] = '\0';
	ch = vtl_changer_get_instance(load_req.instance);
	if (!ch)
		return -ENODEV;
	return vtl_changer_load_slot_to_drive(ch, load_req.slot, load_req.drive,
					      load_req.tape_name, load_req.barcode);
}

static long vtl_ioctl_unload_tape(void __user *uarg)
{
	struct vtl_unload_req unload_req;
	struct vtl_changer *ch;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&unload_req, uarg, sizeof(unload_req)))
		return -EFAULT;
	ch = vtl_changer_get_instance(unload_req.instance);
	if (!ch)
		return -ENODEV;
	if (unload_req.slot < 0)
		return -EINVAL;
	return vtl_changer_unload_drive_to_slot(ch, unload_req.drive,
						unload_req.slot);
}

static long vtl_ioctl_slot_place(void __user *uarg)
{
	struct vtl_slot_place_req place_req;
	struct vtl_changer *ch;
	struct vtl_tape *tape;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&place_req, uarg, sizeof(place_req)))
		return -EFAULT;
	place_req.tape_name[sizeof(place_req.tape_name) - 1] = '\0';
	place_req.barcode[sizeof(place_req.barcode) - 1] = '\0';
	ch = vtl_changer_get_instance(place_req.instance);
	if (!ch)
		return -ENODEV;
	tape = vtl_tape_open_existing(place_req.tape_name);
	if (IS_ERR(tape))
		return PTR_ERR(tape);
	if (!tape)
		return -EINVAL;
	if (place_req.barcode[0])
		vtl_tape_set_barcode(tape, place_req.barcode);
	return vtl_changer_slot_place(ch, place_req.slot, tape);
}

static long vtl_ioctl_move_medium(void __user *uarg)
{
	struct vtl_move_req move_req;
	struct vtl_changer *ch;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&move_req, uarg, sizeof(move_req)))
		return -EFAULT;
	ch = vtl_changer_get_instance(move_req.instance);
	if (!ch)
		return -ENODEV;
	return vtl_changer_move_medium(ch, move_req.src, move_req.dst);
}

static long vtl_ioctl_elem_remove(void __user *uarg)
{
	struct vtl_elem_remove_req remove_req;
	struct vtl_changer *ch;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&remove_req, uarg, sizeof(remove_req)))
		return -EFAULT;
	ch = vtl_changer_get_instance(remove_req.instance);
	if (!ch)
		return -ENODEV;
	return vtl_changer_remove_medium(ch, remove_req.element);
}

static long vtl_ioctl_set_instances(void __user *uarg)
{
	struct vtl_set_instances_ioctl req;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&req, uarg, sizeof(req)))
		return -EFAULT;
	req.spec[VTL_INST_SPEC_MAX - 1] = '\0';
	return vtl_apply_instances_spec(req.spec);
}

static long vtl_ioctl_resize_geometry(void __user *uarg)
{
	struct vtl_set_instances_ioctl req;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&req, uarg, sizeof(req)))
		return -EFAULT;
	req.spec[VTL_INST_SPEC_MAX - 1] = '\0';
	return vtl_apply_geom_resize_only(req.spec);
}

static long vtl_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	void __user *uarg = (void __user *)arg;

	switch (cmd) {
	case VTL_IOCTL_CREATE_TAPE:
		return vtl_ioctl_create_tape(uarg);
	case VTL_IOCTL_LOAD_TAPE:
		return vtl_ioctl_load_tape(uarg);
	case VTL_IOCTL_UNLOAD_TAPE:
		return vtl_ioctl_unload_tape(uarg);
	case VTL_IOCTL_SLOT_PLACE:
		return vtl_ioctl_slot_place(uarg);
	case VTL_IOCTL_MOVE_MEDIUM:
		return vtl_ioctl_move_medium(uarg);
	case VTL_IOCTL_ELEM_REMOVE:
		return vtl_ioctl_elem_remove(uarg);
	case VTL_IOCTL_GET_INVENTORY:
		return vtl_ioctl_get_inventory(uarg);
	case VTL_IOCTL_STATUS:
		return -EOPNOTSUPP;
	case VTL_IOCTL_SET_INSTANCES:
		return vtl_ioctl_set_instances(uarg);
	case VTL_IOCTL_RESIZE_GEOMETRY:
		return vtl_ioctl_resize_geometry(uarg);
	default:
		return -ENOTTY;
	}
}

static const struct file_operations vtl_misc_fops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = vtl_ioctl,
	.compat_ioctl = vtl_ioctl,
};

static struct miscdevice vtl_misc_device = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "vtl",
	.fops = &vtl_misc_fops,
};

int vtl_misc_init(void)
{
	int error;

	error = misc_register(&vtl_misc_device);
	if (error) {
		pr_err("VTL: Failed to register misc device\n");
		return error;
	}

	pr_info("VTL: Misc device registered at /dev/vtl\n");
	return 0;
}

void vtl_misc_exit(void)
{
	misc_deregister(&vtl_misc_device);
	pr_info("VTL: Misc device unregistered\n");
}
