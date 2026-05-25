#ifndef _VTL_H
#define _VTL_H

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/mutex.h>
#include <linux/kref.h>
#include <linux/list.h>
#include <linux/workqueue.h>
#include <linux/delay.h>
#include <linux/blkdev.h>
#include <linux/blk-mq.h>
#include <linux/version.h>

#if LINUX_VERSION_CODE < KERNEL_VERSION(4, 18, 0)
#error "VTL requires Linux kernel 4.18 or later (this tree is too old)"
#endif

#include <scsi/scsi.h>
#include <scsi/scsi_host.h>
#include <scsi/scsi_cmnd.h>
#include <scsi/scsi_device.h>
#include <scsi/scsi_tcq.h>

/*
 * Linux 4.18–6.4: scsi_cmnd has ->scsi_done callback.
 * Linux 6.5+ removes that member; LLDDs must call scsi_done(cmd).
 */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 5, 0)
#define vtl_scsi_done(cmd) scsi_done(cmd)
#else
#define vtl_scsi_done(cmd) ((cmd)->scsi_done(cmd))
#endif

#define VTL_VERSION "1.0.0"
#define VTL_NAME "vtl"

#define VTL_MAX_DRIVES 8
#define VTL_MAX_SLOTS 256
#define VTL_MAX_MAILSLOTS 4
/** Max medium-changer + tape host groups (one platform device each). */
#define VTL_MAX_SCSI_INSTANCES 8
/*
 * Product caps (Plan B / vtladm): up to 8 libraries (SCSI hosts), each with
 * one medium changer (LUN 0), up to 8 tape drives and 256 data slots.
 */

#define VTL_DEFAULT_DRIVES 1
#define VTL_DEFAULT_SLOTS 10

/** Max bytes for `vtl_instances` spec passed via `VTL_IOCTL_SET_INSTANCES` (incl. NUL). */
#define VTL_INST_SPEC_MAX 384

#define VTL_MIN_BLOCK_SIZE 512
#define VTL_MAX_BLOCK_SIZE (1024 * 1024)
#define VTL_DEFAULT_BLOCK_SIZE 32768

#define VTL_MIN_TAPE_SIZE (10 * 1024 * 1024)
#define VTL_MAX_TAPE_SIZE (10ULL * 1024 * 1024 * 1024 * 1024)

#define VTL_VENDOR_ID "VTL     "
/* 16-byte inquiry product: keep changer vs tape distinct for upper layers / devinfo */
#define VTL_PRODUCT_CHANGER "VTL CHANGER     "
#define VTL_PRODUCT_TAPE    "VTL TAPE DRV    "
#define VTL_REVISION "1.00"

/** SCSI element addresses (SMC-3 style; matches vtladm DB slot 0..N-1, drive 1000+i, IE 2000+i). */
#define VTL_ELEM_DRIVE_BASE 1000
#define VTL_ELEM_IE_BASE 2000

/** SMC-3 element type codes in READ ELEMENT STATUS pages (not the same as SCSI device types). */
#define VTL_SMC_ELEM_ST 0x02 /* storage */
#define VTL_SMC_ELEM_IE 0x03 /* import/export */
#define VTL_SMC_ELEM_DT 0x04 /* data transfer (tape drives) */

struct vtl_sense_data {
    u8 key;
    u8 asc;
    u8 ascq;
};

struct vtl_filemark {
    loff_t position;
    u32 index;
};

struct vtl_tape_metadata {
    char serial[32];
    char barcode[16];
    u64 capacity;
    u64 used;
    u32 block_size;
    u32 num_filemarks;
    struct vtl_filemark *filemarks;
    time64_t created;
    time64_t accessed;
    u32 num_snapshots;
    /* Host-side usage counters (SCSI LOG SENSE / health); not persisted to image header today */
    u64 log_bytes_read;
    u64 log_bytes_written;
    u32 mount_count;
};

struct vtl_tape {
    char name[64];
    char path[256];
    struct vtl_tape_metadata meta;
    struct file *file;
    loff_t position;
    bool loaded;
    bool write_protected;
    struct mutex lock;
    struct kref ref;
};

struct vtl_drive {
    int id;
    char name[32];
    struct vtl_tape *loaded_tape;
    struct scsi_device *sdev;
    struct request_queue *queue;
    /* Reserved for future blk layer integration; not used by current LLD. */
    struct blk_mq_tag_set tag_set;
    u32 block_size;
    u8 density;
    bool at_filemark;
    bool at_end;
    bool at_bot;
    struct vtl_sense_data sense;
    struct mutex lock;
    struct work_struct work;
};

struct vtl_slot {
    int id;
    bool occupied;
    struct vtl_tape *tape;
};

struct vtl_changer {
    int id;
    char name[32];
    struct scsi_device *sdev;
    int num_drives;
    int num_slots;
    int num_mailslots;
    struct vtl_drive drives[VTL_MAX_DRIVES];
    struct vtl_slot slots[VTL_MAX_SLOTS];
    struct vtl_slot mailslots[VTL_MAX_MAILSLOTS];
    struct vtl_sense_data sense;
    struct mutex lock;
};

struct vtl_host {
    struct Scsi_Host *shost;
    struct vtl_changer *changer;
    struct list_head list;
    /** Back-pointer for deferred scsi_add_host (outside probe). */
    struct platform_device *pdev;
    /** True after deferred scsi_add_host succeeds. */
    bool scsi_registered;
    /** Deferred scsi_add_host on system_long_wq (probe must not call add_host). */
    struct delayed_work scan_work;
    /** After scsi_add_host: optional extra wait before scsi_scan_host (Kylin). */
    struct delayed_work post_add_scan_work;
    /** Set after scsi_scan_host (+ quiesce) completes or bringup aborted. */
    bool scan_done;
};

int vtl_scsi_queuecommand(struct Scsi_Host *shost, struct scsi_cmnd *cmd);
int vtl_slave_alloc(struct scsi_device *sdev);
void vtl_slave_destroy(struct scsi_device *sdev);
int vtl_slave_configure(struct scsi_device *sdev);
int vtl_change_queue_depth(struct scsi_device *sdev, int depth);

int vtl_tape_create(const char *name, u64 size);
struct vtl_tape *vtl_tape_find_by_name(const char *name);
struct vtl_tape *vtl_tape_open_existing(const char *name);
void vtl_tape_set_barcode(struct vtl_tape *tape, const char *barcode);
int vtl_changer_slot_place(struct vtl_changer *ch, int slot, struct vtl_tape *tape);
int vtl_changer_load_slot_to_drive(struct vtl_changer *ch, int slot, int drive,
				   const char *tape_name, const char *barcode);
int vtl_changer_unload_drive_to_slot(struct vtl_changer *ch, int drive, int slot);
struct vtl_changer *vtl_changer_get_instance(int instance);
int vtl_tape_load(struct vtl_drive *drive, struct vtl_tape *tape);
int vtl_tape_unload(struct vtl_drive *drive);
int vtl_tape_read(struct vtl_drive *drive, u8 *buffer, u32 len, u32 *actual);
int vtl_tape_write(struct vtl_drive *drive, const u8 *buffer, u32 len, u32 *actual);
int vtl_tape_space(struct vtl_drive *drive, int code, int count);
int vtl_tape_write_filemarks(struct vtl_drive *drive, int count);
int vtl_tape_rewind(struct vtl_drive *drive);

void vtl_changer_clear_media(struct vtl_changer *ch);
void vtl_tapes_release_all(void);
/** Drop one reference; frees tape when last ref (module unload / table remove). */
void vtl_tape_put(struct vtl_tape *tape);

int vtl_changer_move_medium(struct vtl_changer *ch, int src, int dst);
/** Remove medium from changer element without placing elsewhere (shelf / off-line). */
int vtl_changer_remove_medium(struct vtl_changer *ch, int elem);
int vtl_changer_exchange_medium(struct vtl_changer *ch, int src1, int src2, int dst);
int vtl_changer_read_element_status(struct vtl_changer *ch, u8 *buffer, u32 len,
				    bool voltag, u8 elem_type, int start_elem,
				    int num_elems);
int vtl_changer_collect_inventory(struct vtl_changer *ch, int *num_drives,
				  int *num_slots, int *num_mailslots,
				  int *count, int *truncated, int elements[],
				  char names[][64], int max_items);

void vtl_set_sense(struct vtl_sense_data *sense, u8 key, u8 asc, u8 ascq);
void vtl_build_sense_buffer(struct scsi_cmnd *cmd, struct vtl_sense_data *sense);

int vtl_sysfs_init(void);
void vtl_sysfs_exit(void);

int vtl_misc_init(void);
void vtl_misc_exit(void);

/**
 * Rebuild SCSI hosts from a `vtl_instances`-format string (e.g. "2x32,1x10") without
 * unloading the module. Requires CAP_SYS_ADMIN and module_param allow_hot_geom=Y (default N).
 * When allow_hot_geom=N returns -EBUSY; prefer insmod vtl_instances= or full module reload.
 */
int vtl_apply_instances_spec(const char *spec);

/**
 * Plan B: adjust per-host drives/slots without platform_device teardown.
 * @spec must have the same segment count as live vtl_ninstances; returns -EINVAL if not.
 * Refuses shrink while tapes remain in removed drives/slots (-EBUSY).
 */
int vtl_apply_geom_resize_only(const char *spec);

/** True during SET_INSTANCES teardown or module unload (SCSI queuecommand should fail fast). */
bool vtl_reconfig_in_progress(void);

/** True while `rmmod` is tearing the module down (deferred bringup/scan must not run). */
bool vtl_module_is_unloading(void);

/* Optional delay after each successful medium move (ms); module_param in vtl_main.c */
extern int vtl_move_delay_ms;

/** Directory for `name.vtltape` files; module_param `tape_dir` in vtl_main.c */
extern char *vtl_tape_dir;

#endif
