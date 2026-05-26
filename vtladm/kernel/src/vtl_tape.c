#include "../include/vtl.h"
#include <linux/err.h>
#include <linux/fs.h>
#include <linux/uaccess.h>
#include <linux/file.h>
#include <linux/limits.h>

static struct vtl_tape *vtl_tapes[VTL_MAX_SLOTS];
static DEFINE_MUTEX(vtl_tape_lock);

static void vtl_tape_release(struct kref *ref)
{
	struct vtl_tape *t = container_of(ref, struct vtl_tape, ref);

	mutex_lock(&t->lock);
	kfree(t->meta.filemarks);
	t->meta.filemarks = NULL;
	t->meta.num_filemarks = 0;
	if (t->file && !IS_ERR(t->file)) {
		filp_close(t->file, NULL);
		t->file = NULL;
	}
	mutex_unlock(&t->lock);
	kfree(t);
}

void vtl_tape_put(struct vtl_tape *tape)
{
	if (tape)
		kref_put(&tape->ref, vtl_tape_release);
}

static int vtl_tape_table_insert(struct vtl_tape *tape)
{
	int i;

	mutex_lock(&vtl_tape_lock);
	for (i = 0; i < VTL_MAX_SLOTS; i++) {
		if (!vtl_tapes[i]) {
			vtl_tapes[i] = tape;
			mutex_unlock(&vtl_tape_lock);
			return 0;
		}
	}
	mutex_unlock(&vtl_tape_lock);
	return -ENOSPC;
}

static void vtl_format_tape_path(char *buf, size_t len, const char *name)
{
	const char *dir = vtl_tape_dir ? vtl_tape_dir : "/opt/vtladm/var/tapes";
	size_t dlen = strnlen(dir, 200);

	while (dlen > 0 && (dir[dlen - 1] == '/' || dir[dlen - 1] == '\n' ||
			    dir[dlen - 1] == '\r' || dir[dlen - 1] == ' ' ||
			    dir[dlen - 1] == '\t'))
		dlen--;
	snprintf(buf, len, "%.*s/%s.vtltape", (int)dlen, dir, name);
}

static void vtl_generate_serial(char *buf, int len)
{
    static atomic64_t counter = ATOMIC64_INIT(0);
    u64 val = atomic64_inc_return(&counter);
    snprintf(buf, len, "VTL%08llX", val);
}

static void vtl_generate_barcode(char *buf, int len)
{
    static atomic64_t counter = ATOMIC64_INIT(0);
    u64 val = atomic64_inc_return(&counter);
    snprintf(buf, len, "VTL%06llX", val);
}

int vtl_tape_create(const char *name, u64 size)
{
    struct vtl_tape *tape;
    struct file *filp;
    char path[256];
    int ret;

    if (size < VTL_MIN_TAPE_SIZE)
        size = VTL_MIN_TAPE_SIZE;
    if (size > VTL_MAX_TAPE_SIZE)
        size = VTL_MAX_TAPE_SIZE;

    tape = kzalloc(sizeof(*tape), GFP_KERNEL);
    if (!tape)
        return -ENOMEM;

    strncpy(tape->name, name, sizeof(tape->name) - 1);
    vtl_format_tape_path(path, sizeof(path), name);
    strncpy(tape->path, path, sizeof(tape->path) - 1);

    filp = filp_open(path, O_CREAT | O_RDWR | O_TRUNC | O_LARGEFILE, 0644);
    if (IS_ERR(filp)) {
        ret = PTR_ERR(filp);
        kfree(tape);
        return ret;
    }

    if (vfs_fallocate(filp, 0, 0, size) < 0) {
        filp_close(filp, NULL);
        kfree(tape);
        return -EIO;
    }

    tape->file = filp;
    tape->position = 0;
    tape->loaded = false;
    tape->write_protected = false;
    mutex_init(&tape->lock);

    vtl_generate_serial(tape->meta.serial, sizeof(tape->meta.serial));
    vtl_generate_barcode(tape->meta.barcode, sizeof(tape->meta.barcode));
    tape->meta.capacity = size;
    tape->meta.used = 0;
    tape->meta.block_size = VTL_DEFAULT_BLOCK_SIZE;
    tape->meta.num_filemarks = 0;
    tape->meta.created = ktime_get_real_seconds();
    tape->meta.accessed = tape->meta.created;
    tape->meta.num_snapshots = 0;
    tape->meta.filemarks = NULL;
    kref_init(&tape->ref);

    mutex_lock(&vtl_tape_lock);
    for (ret = 0; ret < VTL_MAX_SLOTS; ret++) {
        if (!vtl_tapes[ret]) {
            vtl_tapes[ret] = tape;
            break;
        }
    }
    mutex_unlock(&vtl_tape_lock);

    if (ret >= VTL_MAX_SLOTS) {
        filp_close(filp, NULL);
        kfree(tape);
        return -ENOSPC;
    }

    pr_info("VTL: Created tape %s (size %llu)\n", name, size);
    return 0;
}

struct vtl_tape *vtl_tape_find_by_name(const char *name)
{
    int i;
    struct vtl_tape *t;

    if (!name || !name[0])
        return NULL;

    mutex_lock(&vtl_tape_lock);
    for (i = 0; i < VTL_MAX_SLOTS; i++) {
        t = vtl_tapes[i];
        if (t && !strcmp(t->name, name)) {
            mutex_unlock(&vtl_tape_lock);
            return t;
        }
    }
    mutex_unlock(&vtl_tape_lock);
    return NULL;
}

struct vtl_tape *vtl_tape_open_existing(const char *name)
{
    struct vtl_tape *tape;
    struct file *filp;
    char path[256];

    tape = vtl_tape_find_by_name(name);
    if (tape)
        return tape;

    if (!name || !name[0])
        return NULL;

    tape = kzalloc(sizeof(*tape), GFP_KERNEL);
    if (!tape)
        return NULL;

    strncpy(tape->name, name, sizeof(tape->name) - 1);
    vtl_format_tape_path(path, sizeof(path), name);
    strncpy(tape->path, path, sizeof(tape->path) - 1);

    filp = filp_open(path, O_RDWR | O_LARGEFILE, 0);
    if (IS_ERR(filp)) {
        pr_err("VTL: filp_open %s failed: %ld\n", path, PTR_ERR(filp));
        kfree(tape);
        return ERR_PTR(PTR_ERR(filp));
    }

    tape->file = filp;
    tape->position = 0;
    tape->loaded = false;
    tape->write_protected = false;
    mutex_init(&tape->lock);
    memset(&tape->meta, 0, sizeof(tape->meta));
    strncpy(tape->meta.serial, name, sizeof(tape->meta.serial) - 1);
    strncpy(tape->meta.barcode, name, min_t(size_t, 8, strlen(name)));
    tape->meta.block_size = VTL_DEFAULT_BLOCK_SIZE;
    tape->meta.created = ktime_get_real_seconds();
    tape->meta.accessed = tape->meta.created;
    kref_init(&tape->ref);

    if (vtl_tape_table_insert(tape) < 0) {
        vtl_tape_put(tape);
        return ERR_PTR(-ENOSPC);
    }
    pr_info("VTL: Opened existing tape %s\n", name);
    return tape;
}

void vtl_tape_set_barcode(struct vtl_tape *tape, const char *barcode)
{
    size_t n;

    if (!tape || !barcode || !barcode[0])
        return;
    mutex_lock(&tape->lock);
    memset(tape->meta.barcode, 0, sizeof(tape->meta.barcode));
    n = strnlen(barcode, sizeof(tape->meta.barcode) - 1);
    memcpy(tape->meta.barcode, barcode, n);
    mutex_unlock(&tape->lock);
}

int vtl_changer_slot_place(struct vtl_changer *ch, int slot, struct vtl_tape *tape)
{
    struct vtl_slot *dst_slot;

    if (!ch || !tape || slot < 0 || slot >= ch->num_slots)
        return -EINVAL;

    mutex_lock(&ch->lock);
    dst_slot = &ch->slots[slot];
    if (dst_slot->occupied && dst_slot->tape != tape) {
        mutex_unlock(&ch->lock);
        return -EBUSY;
    }
    dst_slot->tape = tape;
    dst_slot->occupied = true;
    mutex_unlock(&ch->lock);
    return 0;
}

int vtl_changer_load_slot_to_drive(struct vtl_changer *ch, int slot, int drive,
				   const char *tape_name, const char *barcode)
{
    struct vtl_tape *tape;
    int ret;

    if (!ch || slot < 0 || slot >= ch->num_slots || drive < 0 ||
	    drive >= ch->num_drives)
        return -EINVAL;

    mutex_lock(&ch->lock);
    if (ch->slots[slot].occupied && ch->slots[slot].tape) {
        if (tape_name && tape_name[0] &&
            strncmp(ch->slots[slot].tape->name, tape_name,
                    sizeof(ch->slots[slot].tape->name)) != 0) {
            mutex_unlock(&ch->lock);
            return -EBUSY;
        }
        if (barcode && barcode[0])
            vtl_tape_set_barcode(ch->slots[slot].tape, barcode);
        mutex_unlock(&ch->lock);
    } else {
        mutex_unlock(&ch->lock);
        if (!tape_name || !tape_name[0])
            return -ENODEV;
        tape = vtl_tape_open_existing(tape_name);
        if (IS_ERR(tape))
            return PTR_ERR(tape);
        if (!tape)
            return -ENODEV;
        if (barcode && barcode[0])
            vtl_tape_set_barcode(tape, barcode);
        ret = vtl_changer_slot_place(ch, slot, tape);
        if (ret)
            return ret;
    }

    return vtl_changer_move_medium(ch, slot, VTL_ELEM_DRIVE_BASE + drive);
}

int vtl_changer_unload_drive_to_slot(struct vtl_changer *ch, int drive, int slot)
{
    if (!ch || drive < 0 || drive >= ch->num_drives)
        return -EINVAL;
    if (slot < 0 || slot >= ch->num_slots)
        return -EINVAL;

    return vtl_changer_move_medium(ch, VTL_ELEM_DRIVE_BASE + drive, slot);
}

void vtl_changer_clear_media(struct vtl_changer *ch)
{
    int i;

    if (!ch)
        return;

    /* Do not take ch->lock while holding drv->lock (move_medium takes ch->lock first). */
    for (i = 0; i < ch->num_drives; i++) {
        struct vtl_drive *d = &ch->drives[i];
        struct vtl_tape *t;

        mutex_lock(&d->lock);
        t = d->loaded_tape;
        if (t) {
            mutex_lock(&t->lock);
            d->loaded_tape = NULL;
            t->loaded = false;
            mutex_unlock(&t->lock);
        }
        mutex_unlock(&d->lock);
    }

    mutex_lock(&ch->lock);
    for (i = 0; i < ch->num_slots; i++) {
        ch->slots[i].tape = NULL;
        ch->slots[i].occupied = false;
    }
    for (i = 0; i < ch->num_mailslots; i++) {
        ch->mailslots[i].tape = NULL;
        ch->mailslots[i].occupied = false;
    }
    mutex_unlock(&ch->lock);
}

void vtl_tapes_release_all(void)
{
    int i;

    mutex_lock(&vtl_tape_lock);
    for (i = 0; i < VTL_MAX_SLOTS; i++) {
        struct vtl_tape *t = vtl_tapes[i];

        if (!t)
            continue;
        vtl_tapes[i] = NULL;
        vtl_tape_put(t);
    }
    mutex_unlock(&vtl_tape_lock);
}

int vtl_tape_load(struct vtl_drive *drv, struct vtl_tape *tape)
{
    mutex_lock(&drv->lock);
    mutex_lock(&tape->lock);

    if (drv->loaded_tape) {
        mutex_unlock(&tape->lock);
        mutex_unlock(&drv->lock);
        return -EBUSY;
    }

    drv->loaded_tape = tape;
    tape->loaded = true;
    tape->position = 0;
    drv->at_bot = true;
    drv->at_end = false;
    drv->at_filemark = false;

    tape->meta.accessed = ktime_get_real_seconds();
    tape->meta.mount_count++;

    mutex_unlock(&tape->lock);
    mutex_unlock(&drv->lock);

    pr_info("VTL: Loaded tape %s into drive %d\n", tape->name, drv->id);
    return 0;
}

int vtl_tape_unload(struct vtl_drive *drv)
{
    struct vtl_tape *tape;

    mutex_lock(&drv->lock);

    tape = drv->loaded_tape;
    if (!tape) {
        mutex_unlock(&drv->lock);
        return -ENODEV;
    }

    mutex_lock(&tape->lock);

    drv->loaded_tape = NULL;
    tape->loaded = false;

    mutex_unlock(&tape->lock);
    mutex_unlock(&drv->lock);

    pr_info("VTL: Unloaded tape %s from drive %d\n", tape->name, drv->id);
    return 0;
}

int vtl_tape_read(struct vtl_drive *drv, u8 *buffer, u32 len, u32 *actual)
{
    struct vtl_tape *tape;
    ssize_t ret;
    loff_t pos;

    mutex_lock(&drv->lock);
    tape = drv->loaded_tape;
    if (!tape) {
        mutex_unlock(&drv->lock);
        return -ENODEV;
    }
    mutex_lock(&tape->lock);

    if (drv->at_filemark) {
        drv->at_filemark = false;
        *actual = 0;
        mutex_unlock(&tape->lock);
        mutex_unlock(&drv->lock);
        return 0;
    }

    pos = tape->position;
    if (pos >= tape->meta.capacity) {
        drv->at_end = true;
        *actual = 0;
        mutex_unlock(&tape->lock);
        mutex_unlock(&drv->lock);
        return 0;
    }

    if (pos + len > tape->meta.capacity)
        len = tape->meta.capacity - pos;

    ret = kernel_read(tape->file, buffer, len, &pos);
    if (ret < 0) {
        mutex_unlock(&tape->lock);
        mutex_unlock(&drv->lock);
        return -EIO;
    }

    tape->position = pos;
    *actual = ret;
    drv->at_bot = (pos == 0);
    drv->at_end = (pos >= tape->meta.capacity);
    tape->meta.accessed = ktime_get_real_seconds();
    tape->meta.log_bytes_read += (u64)ret;

    mutex_unlock(&tape->lock);
    mutex_unlock(&drv->lock);
    return 0;
}

int vtl_tape_write(struct vtl_drive *drv, const u8 *buffer, u32 len, u32 *actual)
{
    struct vtl_tape *tape;
    ssize_t ret;
    loff_t pos;
    u32 to_write;

    mutex_lock(&drv->lock);
    tape = drv->loaded_tape;
    if (!tape) {
        mutex_unlock(&drv->lock);
        return -ENODEV;
    }
    if (tape->write_protected) {
        mutex_unlock(&drv->lock);
        return -EROFS;
    }
    mutex_lock(&tape->lock);

    pos = tape->position;
    if (actual)
        *actual = 0;
    if (pos >= tape->meta.capacity) {
        drv->at_end = true;
        mutex_unlock(&tape->lock);
        mutex_unlock(&drv->lock);
        return -ENOSPC;
    }
    if ((u64)len > (u64)(tape->meta.capacity - pos))
        to_write = (u32)(tape->meta.capacity - pos);
    else
        to_write = len;

    ret = kernel_write(tape->file, buffer, to_write, &pos);
    if (ret < 0) {
        mutex_unlock(&tape->lock);
        mutex_unlock(&drv->lock);
        return -EIO;
    }

    tape->position = pos;
    if (actual)
        *actual = (u32)ret;
    if (pos > tape->meta.used)
        tape->meta.used = pos;
    drv->at_bot = (pos == 0);
    drv->at_end = (pos >= tape->meta.capacity);
    drv->at_filemark = false;
    tape->meta.accessed = ktime_get_real_seconds();
    if (ret > 0)
        tape->meta.log_bytes_written += (u64)ret;

    mutex_unlock(&tape->lock);
    mutex_unlock(&drv->lock);
    return (ret == len) ? 0 : -ENOSPC;
}

int vtl_tape_space(struct vtl_drive *drv, int code, int count)
{
    struct vtl_tape *tape;
    loff_t delta;
    loff_t new_pos;
    s64 max_blocks;
    int ret = 0;

    mutex_lock(&drv->lock);
    tape = drv->loaded_tape;
    if (!tape) {
        mutex_unlock(&drv->lock);
        return -ENODEV;
    }

    mutex_lock(&tape->lock);

    switch (code) {
    case 0:
        if (drv->block_size == 0) {
            ret = -EINVAL;
            break;
        }
        max_blocks = S64_MAX / (s64)drv->block_size;
        if ((s64)count > max_blocks || (s64)count < -max_blocks) {
            ret = -EINVAL;
            break;
        }
        delta = (loff_t)((s64)count * (s64)drv->block_size);
        if ((delta > 0 && tape->position > S64_MAX - delta) ||
            (delta < 0 && tape->position < S64_MIN - delta)) {
            ret = -EINVAL;
            break;
        }
        new_pos = tape->position + delta;
        tape->position = new_pos;
        break;
    case 1:
        drv->at_filemark = (count != 0);
        break;
    case 2:
        drv->at_filemark = (count != 0);
        break;
    case 3:
        tape->position = tape->meta.capacity;
        drv->at_end = true;
        break;
    case 4:
        drv->at_filemark = true;
        break;
    default:
        ret = -EINVAL;
        break;
    }

    if (ret) {
        mutex_unlock(&tape->lock);
        mutex_unlock(&drv->lock);
        return ret;
    }

    if (tape->position < 0)
        tape->position = 0;
    if (tape->position > tape->meta.capacity)
        tape->position = tape->meta.capacity;

    drv->at_bot = (tape->position == 0);
    drv->at_end = (tape->position >= tape->meta.capacity);
    tape->meta.accessed = ktime_get_real_seconds();

    mutex_unlock(&tape->lock);
    mutex_unlock(&drv->lock);
    return 0;
}

int vtl_tape_write_filemarks(struct vtl_drive *drv, int count)
{
    struct vtl_tape *tape;

    mutex_lock(&drv->lock);
    tape = drv->loaded_tape;
    if (!tape) {
        mutex_unlock(&drv->lock);
        return -ENODEV;
    }

    if (tape->write_protected) {
        mutex_unlock(&drv->lock);
        return -EROFS;
    }

    mutex_lock(&tape->lock);
    drv->at_filemark = true;
    tape->meta.accessed = ktime_get_real_seconds();
    mutex_unlock(&tape->lock);
    mutex_unlock(&drv->lock);

    return 0;
}

int vtl_tape_rewind(struct vtl_drive *drv)
{
    struct vtl_tape *tape;

    mutex_lock(&drv->lock);
    tape = drv->loaded_tape;
    if (!tape) {
        mutex_unlock(&drv->lock);
        return -ENODEV;
    }

    mutex_lock(&tape->lock);
    tape->position = 0;
    drv->at_bot = true;
    drv->at_end = false;
    drv->at_filemark = false;
    tape->meta.accessed = ktime_get_real_seconds();
    mutex_unlock(&tape->lock);
    mutex_unlock(&drv->lock);

    return 0;
}

int vtl_changer_move_medium(struct vtl_changer *ch, int src, int dst)
{
    struct vtl_tape *t = NULL;
    int ret = 0;

    mutex_lock(&ch->lock);

    if (src < 1000) {
        struct vtl_slot *src_slot;

        if (src < 0 || src >= ch->num_slots) {
            ret = -EINVAL;
            goto out;
        }
        src_slot = &ch->slots[src];
        if (!src_slot->occupied || !src_slot->tape) {
            ret = -ENODEV;
            goto out;
        }
        t = src_slot->tape;
        src_slot->tape = NULL;
        src_slot->occupied = false;
	    } else if (src < VTL_ELEM_IE_BASE) {
	        struct vtl_drive *src_drv;
	        int di = src - VTL_ELEM_DRIVE_BASE;

        if (di < 0 || di >= ch->num_drives) {
            ret = -EINVAL;
            goto out;
	        }
	        src_drv = &ch->drives[di];
	        mutex_lock(&src_drv->lock);
	        if (!src_drv->loaded_tape) {
	            mutex_unlock(&src_drv->lock);
	            ret = -ENODEV;
	            goto out;
	        }
	        t = src_drv->loaded_tape;
	        src_drv->loaded_tape = NULL;
	        t->loaded = false;
	        src_drv->at_filemark = false;
	        src_drv->at_end = false;
	        src_drv->at_bot = true;
	        mutex_unlock(&src_drv->lock);
    } else if (src >= VTL_ELEM_IE_BASE &&
	       src < VTL_ELEM_IE_BASE + ch->num_mailslots) {
        struct vtl_slot *ms;
        int mi = src - VTL_ELEM_IE_BASE;

        ms = &ch->mailslots[mi];
        if (!ms->occupied || !ms->tape) {
            ret = -ENODEV;
            goto out;
        }
        t = ms->tape;
        ms->tape = NULL;
        ms->occupied = false;
    } else {
        ret = -EINVAL;
        goto out;
    }

    if (dst < 1000) {
        struct vtl_slot *dst_slot;

        if (dst < 0 || dst >= ch->num_slots) {
            ret = -EINVAL;
            goto rollback;
        }
        dst_slot = &ch->slots[dst];
        if (dst_slot->occupied) {
            ret = -EBUSY;
            goto rollback;
        }
        dst_slot->tape = t;
        dst_slot->occupied = true;
    } else if (dst < VTL_ELEM_IE_BASE) {
        struct vtl_drive *dst_drv;
        int di = dst - VTL_ELEM_DRIVE_BASE;

        if (di < 0 || di >= ch->num_drives) {
            ret = -EINVAL;
            goto rollback;
	        }
	        dst_drv = &ch->drives[di];
	        mutex_lock(&dst_drv->lock);
	        if (dst_drv->loaded_tape) {
	            mutex_unlock(&dst_drv->lock);
	            ret = -EBUSY;
	            goto rollback;
	        }
	        dst_drv->loaded_tape = t;
	        t->loaded = true;
	        dst_drv->at_filemark = false;
	        dst_drv->at_end = false;
	        dst_drv->at_bot = true;
	        mutex_unlock(&dst_drv->lock);
    } else if (dst >= VTL_ELEM_IE_BASE &&
	       dst < VTL_ELEM_IE_BASE + ch->num_mailslots) {
        struct vtl_slot *ms;
        int mi = dst - VTL_ELEM_IE_BASE;

        ms = &ch->mailslots[mi];
        if (ms->occupied) {
            ret = -EBUSY;
            goto rollback;
        }
        ms->tape = t;
        ms->occupied = true;
        t->loaded = false;
    } else {
        ret = -EINVAL;
        goto rollback;
    }

    pr_info("VTL: Moved medium from %d to %d\n", src, dst);
    ret = 0;
    goto out;

rollback:
    if (src < 1000 && src >= 0 && src < ch->num_slots) {
        ch->slots[src].tape = t;
        ch->slots[src].occupied = (t != NULL);
    } else if (src >= VTL_ELEM_DRIVE_BASE && src < VTL_ELEM_IE_BASE) {
        int di = src - VTL_ELEM_DRIVE_BASE;

	        if (di >= 0 && di < ch->num_drives && t) {
	            mutex_lock(&ch->drives[di].lock);
	            ch->drives[di].loaded_tape = t;
	            t->loaded = true;
	            ch->drives[di].at_filemark = false;
	            ch->drives[di].at_end = false;
	            ch->drives[di].at_bot = true;
	            mutex_unlock(&ch->drives[di].lock);
	        }
    } else if (src >= VTL_ELEM_IE_BASE &&
	       src < VTL_ELEM_IE_BASE + ch->num_mailslots) {
        int mi = src - VTL_ELEM_IE_BASE;

        if (mi >= 0 && mi < ch->num_mailslots && t) {
            ch->mailslots[mi].tape = t;
            ch->mailslots[mi].occupied = true;
            t->loaded = false;
        }
    }

out:
    mutex_unlock(&ch->lock);
    if (ret == 0 && vtl_move_delay_ms > 0)
        msleep(min_t(unsigned int, (unsigned int)vtl_move_delay_ms, 60000U));
    return ret;
}

int vtl_changer_remove_medium(struct vtl_changer *ch, int elem)
{
    int ret = 0;

    if (!ch)
        return -EINVAL;

    mutex_lock(&ch->lock);

    if (elem < 1000) {
        struct vtl_slot *s;

        if (elem < 0 || elem >= ch->num_slots) {
            ret = -EINVAL;
            goto out;
        }
        s = &ch->slots[elem];
        if (!s->occupied || !s->tape) {
            ret = -ENODEV;
            goto out;
        }
        s->tape = NULL;
        s->occupied = false;
    } else if (elem < VTL_ELEM_IE_BASE) {
        struct vtl_drive *d;
        int di = elem - VTL_ELEM_DRIVE_BASE;

        if (di < 0 || di >= ch->num_drives) {
            ret = -EINVAL;
            goto out;
	        }
	        d = &ch->drives[di];
	        mutex_lock(&d->lock);
	        if (!d->loaded_tape) {
	            mutex_unlock(&d->lock);
	            ret = -ENODEV;
	            goto out;
	        }
	        d->loaded_tape->loaded = false;
	        d->loaded_tape = NULL;
	        d->at_filemark = false;
	        d->at_end = false;
	        d->at_bot = true;
	        mutex_unlock(&d->lock);
    } else if (elem >= VTL_ELEM_IE_BASE &&
	       elem < VTL_ELEM_IE_BASE + ch->num_mailslots) {
        struct vtl_slot *ms;
        int mi = elem - VTL_ELEM_IE_BASE;

        ms = &ch->mailslots[mi];
        if (!ms->occupied || !ms->tape) {
            ret = -ENODEV;
            goto out;
        }
        ms->tape = NULL;
        ms->occupied = false;
    } else {
        ret = -EINVAL;
    }

    if (ret == 0)
        pr_info("VTL: Removed medium from element %d\n", elem);

out:
    mutex_unlock(&ch->lock);
    return ret;
}

int vtl_changer_exchange_medium(struct vtl_changer *ch, int src1, int src2, int dst)
{
    return -EOPNOTSUPP;
}

static u32 vtl_elem_status_desc(u8 *p, u32 buf_left, u8 elem_type, int addr,
				bool full, const char *barcode, bool voltag)
{
    u32 dlen = voltag ? 32U : 12U;
    size_t tag_len;

    if (buf_left < dlen)
        return 0;

    memset(p, 0, dlen);
    /* SMC-3 short descriptor: element address BE at bytes 0-1, Full at byte 2 */
    p[0] = (addr >> 8) & 0xff;
    p[1] = addr & 0xff;
    p[2] = full ? 0x01 : 0x00;
    p[3] = (elem_type & 0x07) << 5;

    if (voltag && full && barcode && barcode[0]) {
        tag_len = strnlen(barcode, 32);
        memcpy(&p[12], barcode, tag_len);
        if (tag_len < 32)
            memset(&p[12] + tag_len, ' ', 32 - tag_len);
    }
    return dlen;
}

static bool vtl_elem_type_wanted(u8 filter, u8 elem_type)
{
    if (!filter || filter > 0x04)
        return true;
    return filter == elem_type;
}

static bool vtl_elem_in_range(int addr, int start, int num)
{
    if (num <= 0)
        return true;
    return addr >= start && addr < start + num;
}

static void vtl_put_be16(u16 v, u8 *p)
{
    p[0] = (v >> 8) & 0xff;
    p[1] = v & 0xff;
}

static void vtl_put_be32(u32 v, u8 *p)
{
    p[0] = (v >> 24) & 0xff;
    p[1] = (v >> 16) & 0xff;
    p[2] = (v >> 8) & 0xff;
    p[3] = v & 0xff;
}

/*
 * Append one SMC-3 Element Status Page (8-byte page header + descriptors).
 * Returns bytes written, or 0 if buffer too small.
 */
static u32 vtl_append_elem_status_page(struct vtl_changer *ch, u8 *p, u32 left,
				       u8 smc_type, bool voltag,
				       int start_elem, int num_elems,
				       u8 cdb_type_filter)
{
    u32 desc_len = voltag ? 32U : 12U;
    u32 page_hdr = 8U;
    u32 desc_bytes = 0;
    u32 written;
    u8 *desc;
    int i;

    if (!vtl_elem_type_wanted(cdb_type_filter, smc_type))
        return 0;

    if (left < page_hdr)
        return 0;

    desc = p + page_hdr;

    if (smc_type == VTL_SMC_ELEM_ST) {
        for (i = 0; i < ch->num_slots; i++) {
            struct vtl_slot *slot = &ch->slots[i];
            u32 n;

            if (!vtl_elem_in_range(i, start_elem, num_elems))
                continue;
            if (left < page_hdr + desc_bytes + desc_len)
                break;
            n = vtl_elem_status_desc(desc + desc_bytes,
					     left - page_hdr - desc_bytes,
					     smc_type, i,
					     slot->occupied && slot->tape != NULL,
					     slot->tape ? slot->tape->meta.barcode : NULL,
					     voltag);
            if (!n)
                break;
            desc_bytes += n;
        }
	    } else if (smc_type == VTL_SMC_ELEM_DT) {
	        for (i = 0; i < ch->num_drives; i++) {
	            struct vtl_drive *drv = &ch->drives[i];
	            struct vtl_tape *tape;
	            char barcode[sizeof(drv->loaded_tape->meta.barcode)];
	            int addr = VTL_ELEM_DRIVE_BASE + i;
	            u32 n;

            if (!vtl_elem_in_range(addr, start_elem, num_elems))
                continue;
	            if (left < page_hdr + desc_bytes + desc_len)
	                break;
	            mutex_lock(&drv->lock);
	            tape = drv->loaded_tape;
	            if (tape)
	                strscpy(barcode, tape->meta.barcode, sizeof(barcode));
	            mutex_unlock(&drv->lock);
	            n = vtl_elem_status_desc(desc + desc_bytes,
						     left - page_hdr - desc_bytes,
						     smc_type, addr,
						     tape != NULL,
						     tape ? barcode : NULL,
						     voltag);
            if (!n)
                break;
            desc_bytes += n;
        }
    } else if (smc_type == VTL_SMC_ELEM_IE) {
        for (i = 0; i < ch->num_mailslots; i++) {
            struct vtl_slot *ms = &ch->mailslots[i];
            int addr = VTL_ELEM_IE_BASE + i;
            u32 n;

            if (!vtl_elem_in_range(addr, start_elem, num_elems))
                continue;
            if (left < page_hdr + desc_bytes + desc_len)
                break;
            n = vtl_elem_status_desc(desc + desc_bytes,
					     left - page_hdr - desc_bytes,
					     smc_type, addr,
					     ms->occupied && ms->tape != NULL,
					     ms->tape ? ms->tape->meta.barcode : NULL,
					     voltag);
            if (!n)
                break;
            desc_bytes += n;
        }
    }

    memset(p, 0, page_hdr);
    /*
     * Element Status Page header byte 0: mtx/ch expect the SMC element type code
     * (2=ST, 3=IE, 4=DT) in the low byte, not (type<<5) in bits 5-7 only.
     * Byte 1: mtx Element2StatusPage uses E2_PVOLTAG (0x80) when voltag set.
     * Per-descriptor byte 3 still uses (type<<5) for initiators that read it.
     */
    p[0] = smc_type & 0x07;
    if (voltag)
        p[1] = 0x80;
    vtl_put_be16((u16)desc_len, &p[2]);
    vtl_put_be32(desc_bytes, &p[4]);

    written = page_hdr + desc_bytes;
    return written;
}

/*
 * Walk Element Status Pages after build; count descriptors and first address
 * for the 8-byte Element Status Data header (mtx decrements byte 2-3 per desc).
 */
static u16 vtl_res_count_descriptors(const u8 *pages, u32 pages_len, u16 *first_addr)
{
    u32 off = 0;
    u16 count = 0;
    u16 first = 0xffff;

    while (off + 8 <= pages_len) {
        u32 dbytes = ((u32)pages[off + 4] << 24) | ((u32)pages[off + 5] << 16) |
		     ((u32)pages[off + 6] << 8) | pages[off + 7];
        u16 dlen = ((u16)pages[off + 2] << 8) | pages[off + 3];

        off += 8;
        if (!dlen) {
            if (dbytes)
                off += dbytes;
            continue;
        }
        {
            u32 dend = off + dbytes;

            while (off + dlen <= dend && off + dlen <= pages_len) {
                u16 addr = ((u16)pages[off] << 8) | pages[off + 1];

                if (first == 0xffff)
                    first = addr;
                count++;
                off += dlen;
            }
        }
    }

    *first_addr = (first == 0xffff) ? 0 : first;
    return count;
}

static void vtl_res_write_data_header(u8 *buffer, u32 pages_len, u16 first_addr,
				     u16 num_desc)
{
    buffer[0] = (first_addr >> 8) & 0xff;
    buffer[1] = first_addr & 0xff;
    buffer[2] = (num_desc >> 8) & 0xff;
    buffer[3] = num_desc & 0xff;
    buffer[4] = 0;
    buffer[5] = (pages_len >> 16) & 0xff;
    buffer[6] = (pages_len >> 8) & 0xff;
    buffer[7] = pages_len & 0xff;
}

int vtl_changer_read_element_status(struct vtl_changer *ch, u8 *buffer, u32 len,
				    bool voltag, u8 elem_type, int start_elem,
				    int num_elems)
{
    u8 *p;
    u32 pages_len = 0;
    u32 total;
    u32 n;

    if (!buffer || len < 8)
        return -EINVAL;

    mutex_lock(&ch->lock);

    p = buffer + 8;

    n = vtl_append_elem_status_page(ch, p, len - 8, VTL_SMC_ELEM_ST, voltag,
				    start_elem, num_elems, elem_type);
    pages_len += n;
    p += n;

    n = vtl_append_elem_status_page(ch, p, len - 8 - pages_len, VTL_SMC_ELEM_DT,
				    voltag, start_elem, num_elems, elem_type);
    pages_len += n;
    p += n;

    n = vtl_append_elem_status_page(ch, p, len - 8 - pages_len, VTL_SMC_ELEM_IE,
				    voltag, start_elem, num_elems, elem_type);
    pages_len += n;

    mutex_unlock(&ch->lock);

    {
        u16 first_addr = 0;
        u16 num_desc = vtl_res_count_descriptors(buffer + 8, pages_len, &first_addr);

        memset(buffer, 0, 8);
        vtl_res_write_data_header(buffer, pages_len, first_addr, num_desc);
    }

    total = 8 + pages_len;
    return min_t(u32, total, len);
}

int vtl_changer_collect_inventory(struct vtl_changer *ch, int *num_drives,
				  int *num_slots, int *num_mailslots,
				  int *count, int *truncated, int elements[],
				  char names[][64], int max_items)
{
    int i, n = 0, total = 0;

    if (!ch || !count || !elements || !names || max_items <= 0)
        return -EINVAL;

    mutex_lock(&ch->lock);
    if (num_drives)
        *num_drives = ch->num_drives;
    if (num_slots)
        *num_slots = ch->num_slots;
    if (num_mailslots)
        *num_mailslots = ch->num_mailslots;

    for (i = 0; i < ch->num_slots; i++) {
        if (ch->slots[i].occupied && ch->slots[i].tape)
            total++;
    }
	    for (i = 0; i < ch->num_drives; i++) {
	        mutex_lock(&ch->drives[i].lock);
	        if (ch->drives[i].loaded_tape)
	            total++;
	        mutex_unlock(&ch->drives[i].lock);
	    }
    for (i = 0; i < ch->num_mailslots; i++) {
        if (ch->mailslots[i].occupied && ch->mailslots[i].tape)
            total++;
    }

    for (i = 0; i < ch->num_slots && n < max_items; i++) {
        if (ch->slots[i].occupied && ch->slots[i].tape) {
            elements[n] = i;
            strscpy(names[n], ch->slots[i].tape->name, 64);
            n++;
        }
    }
	    for (i = 0; i < ch->num_drives && n < max_items; i++) {
	        mutex_lock(&ch->drives[i].lock);
	        if (ch->drives[i].loaded_tape) {
	            elements[n] = VTL_ELEM_DRIVE_BASE + i;
	            strscpy(names[n], ch->drives[i].loaded_tape->name, 64);
	            n++;
	        }
	        mutex_unlock(&ch->drives[i].lock);
	    }
    for (i = 0; i < ch->num_mailslots && n < max_items; i++) {
        if (ch->mailslots[i].occupied && ch->mailslots[i].tape) {
            elements[n] = VTL_ELEM_IE_BASE + i;
            strscpy(names[n], ch->mailslots[i].tape->name, 64);
            n++;
        }
    }
    mutex_unlock(&ch->lock);
    *count = n;
    if (truncated)
        *truncated = total > max_items ? 1 : 0;
    return 0;
}
