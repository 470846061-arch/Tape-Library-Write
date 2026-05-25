#include "../include/vtl.h"
#include <linux/sysfs.h>
#include <linux/kobject.h>

static struct kobject *vtl_kobj;

static ssize_t vtl_show(struct kobject *kobj, struct kobj_attribute *attr, char *buf)
{
    return sprintf(buf, "VTL Virtual Tape Library v%s\n", VTL_VERSION);
}

static ssize_t vtl_create_tape_store(struct kobject *kobj, struct kobj_attribute *attr, const char *buf, size_t count)
{
    char name[64];
    u64 size = 100 * 1024 * 1024;
    int ret;

    if (sscanf(buf, "%63s %llu", name, &size) >= 1) {
        ret = vtl_tape_create(name, size);
        if (ret < 0)
            return ret;
    }

    return count;
}

static struct kobj_attribute vtl_attr = __ATTR_RO(vtl);
/* __ATTR_WO(create_tape) would require a symbol create_tape_store; name the store explicitly. */
static struct kobj_attribute vtl_create_tape_attr =
    __ATTR(create_tape, 0200, NULL, vtl_create_tape_store);

static struct attribute *vtl_attrs[] = {
    &vtl_attr.attr,
    &vtl_create_tape_attr.attr,
    NULL,
};

static struct attribute_group vtl_attr_group = {
    .attrs = vtl_attrs,
};

int vtl_sysfs_init(void)
{
    int error;

    vtl_kobj = kobject_create_and_add("vtl", kernel_kobj);
    if (!vtl_kobj)
        return -ENOMEM;

    error = sysfs_create_group(vtl_kobj, &vtl_attr_group);
    if (error) {
        kobject_put(vtl_kobj);
        return error;
    }

    pr_info("VTL: Sysfs initialized\n");
    return 0;
}

void vtl_sysfs_exit(void)
{
    if (vtl_kobj) {
        sysfs_remove_group(vtl_kobj, &vtl_attr_group);
        kobject_put(vtl_kobj);
    }
    pr_info("VTL: Sysfs exited\n");
}
