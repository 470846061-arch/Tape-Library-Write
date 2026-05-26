#include "../include/vtl.h"
#include "../include/vtl_personality.h"
#include <linux/atomic.h>
#include <linux/completion.h>
#include <linux/jiffies.h>
#include <linux/moduleparam.h>
#include <linux/platform_device.h>
#include <linux/string.h>
#include <linux/workqueue.h>
#include <scsi/scsi_device.h>

/** Non-zero while SET_INSTANCES tears down / rebuilds platform hosts (queuecommand fail-fast). */
static atomic_t vtl_reconfig_active = ATOMIC_INIT(0);
/** Set at start of `rmmod`; blocks deferred bringup/scan and ioctl geom jobs. */
static atomic_t vtl_module_unloading = ATOMIC_INIT(0);
/** Deferred scsi_add_host / scsi_scan_host (flush on unload; not system_long_wq). */
static struct workqueue_struct *vtl_bringup_wq;

static int vtl_num_drives = VTL_DEFAULT_DRIVES;
static int vtl_num_slots = VTL_DEFAULT_SLOTS;
static char *vtl_instances;
/* INQUIRY personality: vtl (default), ibm, stk, hp — see vtl_personality.c */
static char *personality = "vtl";
/* Stable buffer for insmod + SET_INSTANCES; sysfs reads via module_param_cb. */
static char vtl_instances_kern[VTL_INST_SPEC_MAX];

static struct list_head vtl_host_list = LIST_HEAD_INIT(vtl_host_list);
static DEFINE_MUTEX(vtl_list_lock);

bool vtl_reconfig_in_progress(void)
{
	return atomic_read(&vtl_reconfig_active) != 0 ||
	       atomic_read(&vtl_module_unloading) != 0;
}

bool vtl_module_is_unloading(void)
{
	return atomic_read(&vtl_module_unloading) != 0;
}

struct vtl_changer *vtl_changer_get_instance(int instance)
{
    struct vtl_host *vhost;
    struct vtl_changer *ch = NULL;
    int i = 0;

    if (instance < 0)
        return NULL;

    mutex_lock(&vtl_list_lock);
    list_for_each_entry(vhost, &vtl_host_list, list) {
        if (i == instance) {
            ch = vhost->changer;
            break;
        }
        i++;
    }
    mutex_unlock(&vtl_list_lock);
    return ch;
}
static DEFINE_MUTEX(vtl_reconfig_mutex);

/*
 * SET_INSTANCES (ioctl) used to call platform_device_add/probe/sysfs on the ioctl
 * kernel stack. On Kylin 4.19 + VMware we saw stack_segment / __kmalloc_track_caller
 * faults in kernfs during probe. Run the reconfig body on a dedicated single-thread
 * workqueue so sysfs/slab work uses a fresh stack; vtl_geom_q_mutex serializes jobs.
 */
static struct workqueue_struct *vtl_geom_wq;
static DEFINE_MUTEX(vtl_geom_q_mutex);
static struct {
	struct work_struct work;
	struct completion done;
	char spec[VTL_INST_SPEC_MAX];
	int ret;
} vtl_geom_job;

/*
 * Defer scsi_add_host (+ scsi_scan_host) off the platform probe path. Some Kylin
 * 4.19 systems reboot even with noscan=1 while scsi_add_host still ran in probe.
 * scsi_scan_host is queued again after post_add_scan_delay_ms (+ per-host
 * scan_host_stagger_ms * id) so add and scan are not back-to-back and multi-host
 * scans do not all kick do_scan_async together (Kylin 4.19 st_probe race).
 */
static int vtl_scan_delay_ms = 500;
module_param_named(scan_delay_ms, vtl_scan_delay_ms, int, 0644);
/** Milliseconds after successful scsi_add_host before scsi_scan_host (ignored if noscan). */
static int vtl_post_add_scan_delay_ms = 600;
module_param_named(post_add_scan_delay_ms, vtl_post_add_scan_delay_ms, int, 0644);
static bool vtl_noscan;
module_param_named(noscan, vtl_noscan, bool, 0644);
/*
 * When vtl_instances lists multiple hosts, without staggering they all fire
 * scsi_add_host (+ scan) at the same jiffies; some Kylin 4.19 systems still reboot.
 * Per-host delay = scan_delay_ms + (platform_device_id * bringup_stagger_ms).
 */
static int vtl_bringup_stagger_ms = 400;
module_param_named(bringup_stagger_ms, vtl_bringup_stagger_ms, int, 0644);
/*
 * After scsi_add_host, scsi_scan_host queues async do_scan_async work. Several
 * hosts scanning together can overlap st_probe/create_one_cdev on Kylin 4.19
 * (kstrdup in sysfs); extra delay per host index before scsi_scan_host spreads
 * those scans. Total wait = post_add_scan_delay_ms + id * scan_host_stagger_ms.
 */
static int vtl_scan_host_stagger_ms = 3000;
module_param_named(scan_host_stagger_ms, vtl_scan_host_stagger_ms, int, 0644);
/*
 * Kylin 4.19: parallel scsi_scan_host across several VTL hosts can overlap
 * st_probe/create_one_cdev in sysfs (same stack_segment class as ioctl path).
 * serial_scsi_scan mutex serializes scsi_scan_host calls; scan_async_quiesce_ms
 * adds sleep after each scan because scsi_complete_async_scans() is not exported
 * to modules on some Kylin 4.19 trees (MODPOST undefined).
 */
static bool vtl_serial_scsi_scan = true;
module_param_named(serial_scsi_scan, vtl_serial_scsi_scan, bool, 0644);
/*
 * Serialize full add_host + scsi_scan_host + quiesce per host (not just scan).
 * Required for 4+ hosts on Kylin 4.19 — parallel add_host while another host scans
 * caused insmod panics with 8x16x256.
 */
static bool vtl_serial_full_bringup = true;
module_param_named(serial_full_bringup, vtl_serial_full_bringup, bool, 0644);
static DEFINE_MUTEX(vtl_host_scan_mutex);
static DEFINE_MUTEX(vtl_full_bringup_mutex);
/** Milliseconds after each scsi_scan_host (before releasing serial_scsi_scan mutex). */
static int vtl_scan_async_quiesce_ms = 1000;
module_param_named(scan_async_quiesce_ms, vtl_scan_async_quiesce_ms, int, 0644);
/*
 * After SET_INSTANCES tears down platform devices, sysfs/kernfs may still be
 * finishing on Kylin 4.19; immediate platform_device_add can reproduce kmalloc
 * faults with device-name-like garbage in registers. synchronize_rcu() plus an
 * optional sleep (default 200 ms) quiesces before re-registering.
 */
static int vtl_hotgeom_quiesce_ms = 200;
module_param_named(hotgeom_quiesce_ms, vtl_hotgeom_quiesce_ms, int, 0644);
/*
 * Hot SET_INSTANCES rebuilds all platform/SCSI hosts in-process. On Kylin 4.19 / VMware
 * this has caused delayed slab corruption when LIO or backup software still holds sg/st.
 * Default N: ioctl returns -EBUSY; change geometry via rmmod/insmod (vtl-kernelctl reload).
 */
static bool vtl_allow_hot_geom;
module_param_named(allow_hot_geom, vtl_allow_hot_geom, bool, 0644);
/** When allow_hot_geom=Y, refuse SET_INSTANCES if any scsi_device exists on VTL hosts (LIO/export). */
static bool vtl_hotgeom_require_no_sdevs = true;
module_param_named(hotgeom_require_no_sdevs, vtl_hotgeom_require_no_sdevs, bool, 0644);
/** Max ms to wait for deferred bringup/scan before hot reconfigure (0 = do not wait). */
static int vtl_hotgeom_bringup_wait_ms = 120000;
module_param_named(hotgeom_bringup_wait_ms, vtl_hotgeom_bringup_wait_ms, int, 0644);
/** Extra ms after cancelling bringup work before platform_device_unregister (rmmod). */
static int vtl_rmmod_quiesce_ms = 12000;
module_param_named(rmmod_quiesce_ms, vtl_rmmod_quiesce_ms, int, 0644);

static struct scsi_host_template vtl_sht = {
    .module = THIS_MODULE,
    .name = "VTL",
    .proc_name = "vtl",
    .queuecommand = vtl_scsi_queuecommand,
    .slave_alloc = vtl_slave_alloc,
    .slave_destroy = vtl_slave_destroy,
    .slave_configure = vtl_slave_configure,
    .change_queue_depth = vtl_change_queue_depth,
    .cmd_per_lun = 8,
    .can_queue = 32,
    .sg_tablesize = 256,
    .this_id = -1,
    .sg_prot_tablesize = 256,
    .emulated = 1,
};

struct vtl_plat_data {
    int num_drives;
    int num_slots;
};

static int vtl_ninstances;
static int vtl_inst_drives[VTL_MAX_SCSI_INSTANCES];
static int vtl_inst_slots[VTL_MAX_SCSI_INSTANCES];
static struct platform_device **vtl_pdev_tab;

/**
 * Parse comma-separated NxM tokens into parallel drive/slot arrays.
 * @spec non-NULL; if empty or whitespace-only, returns -EINVAL (use module-param path for defaults).
 */
static int vtl_parse_spec_tokens(const char *spec, int *out_ninst,
                 int out_drives[VTL_MAX_SCSI_INSTANCES],
                 int out_slots[VTL_MAX_SCSI_INSTANCES])
{
    int i;
    char *copy, *ctx, *tok;

    if (!spec || !spec[0])
        return -EINVAL;

    copy = kstrdup(spec, GFP_KERNEL);
    if (!copy)
        return -ENOMEM;

    i = 0;
    ctx = copy;
    while ((tok = strsep(&ctx, ",")) != NULL) {
        int d, s;

        while (*tok == ' ' || *tok == '\t')
            tok++;
        if (*tok == '\0')
            continue;
        if (sscanf(tok, "%dx%d", &d, &s) != 2) {
            pr_err("VTL: bad vtl_instances token '%s' (want NxM)\n", tok);
            kfree(copy);
            return -EINVAL;
        }
        if (d < 1)
            d = 1;
        if (d > VTL_MAX_DRIVES)
            d = VTL_MAX_DRIVES;
        if (s < 1)
            s = 1;
        if (s > VTL_MAX_SLOTS)
            s = VTL_MAX_SLOTS;
        if (i >= VTL_MAX_SCSI_INSTANCES) {
            pr_err("VTL: vtl_instances: too many entries (max %d)\n",
                   VTL_MAX_SCSI_INSTANCES);
            kfree(copy);
            return -EINVAL;
        }
        out_drives[i] = d;
        out_slots[i] = s;
        i++;
    }
    kfree(copy);

    if (i == 0) {
        pr_err("VTL: vtl_instances produced zero libraries\n");
        return -EINVAL;
    }
    *out_ninst = i;
    return 0;
}

static void vtl_publish_instances_spec(const char *spec)
{
    if (!spec || !spec[0])
        return;
    strlcpy(vtl_instances_kern, spec, sizeof(vtl_instances_kern));
    vtl_instances = vtl_instances_kern;
}

/** Build comma-separated NxM from live vtl_inst_* and publish to module_param. */
static void vtl_publish_live_instances_spec(void)
{
    char buf[VTL_INST_SPEC_MAX];
    int pos = 0;
    int i;

    if (vtl_ninstances <= 0)
        return;
    for (i = 0; i < vtl_ninstances; i++) {
        int n;

        n = scnprintf(buf + pos, sizeof(buf) - pos, "%s%dx%d",
                  i ? "," : "", vtl_inst_drives[i], vtl_inst_slots[i]);
        if (n <= 0 || pos + n >= (int)sizeof(buf))
            return;
        pos += n;
    }
    vtl_publish_instances_spec(buf);
}

struct vtl_geom_snapshot {
    int ninst;
    int drives[VTL_MAX_SCSI_INSTANCES];
    int slots[VTL_MAX_SCSI_INSTANCES];
    char spec[VTL_INST_SPEC_MAX];
};

static void vtl_save_geometry_snapshot(struct vtl_geom_snapshot *snap)
{
    int i;

    snap->ninst = vtl_ninstances;
    for (i = 0; i < VTL_MAX_SCSI_INSTANCES; i++) {
        snap->drives[i] = vtl_inst_drives[i];
        snap->slots[i] = vtl_inst_slots[i];
    }
    if (vtl_instances && vtl_instances[0])
        strlcpy(snap->spec, vtl_instances, sizeof(snap->spec));
    else
        snap->spec[0] = '\0';
}

static void vtl_restore_geometry_arrays(const struct vtl_geom_snapshot *snap)
{
    int i;

    vtl_ninstances = snap->ninst;
    for (i = 0; i < VTL_MAX_SCSI_INSTANCES; i++) {
        vtl_inst_drives[i] = snap->drives[i];
        vtl_inst_slots[i] = snap->slots[i];
    }
}

/* Upper bound for vtl_wait_all_hosts_bringup_idle during quiesce/rmmod. */
static unsigned int vtl_compute_bringup_drain_ms(void)
{
    unsigned int post = (unsigned int)max_t(int, 0, vtl_post_add_scan_delay_ms);
    unsigned int stagger = (unsigned int)max_t(int, 0, vtl_bringup_stagger_ms);
    unsigned int scan_q = (unsigned int)max_t(int, 0, vtl_scan_async_quiesce_ms);
    unsigned int inst = (unsigned int)max_t(int, 1, vtl_ninstances);
    unsigned int w = post + (inst - 1U) * stagger + scan_q + 5000U;

    if (w < 15000U)
        w = 15000U;
    if (w > 120000U)
        w = 120000U;
    return w;
}

static void vtl_cancel_all_host_delayed_work(void)
{
    struct vtl_host *vhost;

    mutex_lock(&vtl_list_lock);
    list_for_each_entry(vhost, &vtl_host_list, list) {
        cancel_delayed_work_sync(&vhost->scan_work);
        cancel_delayed_work_sync(&vhost->post_add_scan_work);
    }
    mutex_unlock(&vtl_list_lock);

    if (vtl_bringup_wq)
        flush_workqueue(vtl_bringup_wq);
}

static int vtl_wait_all_hosts_bringup_idle(unsigned int max_ms);
static int vtl_register_all_pdevs(void);
static void vtl_changer_free(struct vtl_changer *ch);
static void vtl_unregister_all_hosts(bool release_tape_cache);
static void vtl_destroy_all_pdevs(void);

/*
 * Before unregistering any platform device: stop deferred bringup/scan on every
 * host and scsi_remove_host all. Unregistering host 0 while host 5 still scans
 * caused Kylin/slab panics in insmod/rmmod stress tests.
 *
 * @for_module_exit: when true (`rmmod`), sets vtl_module_unloading and extra msleep.
 */
static void vtl_quiesce_all_hosts(bool for_module_exit)
{
    struct vtl_host *vhost;

    if (for_module_exit)
        atomic_set(&vtl_module_unloading, 1);
    atomic_set(&vtl_reconfig_active, 1);
    smp_wmb();

    vtl_cancel_all_host_delayed_work();
    if (vtl_wait_all_hosts_bringup_idle(vtl_compute_bringup_drain_ms()) != 0)
        pr_warn("VTL: deferred bringup/scan still active after %u ms — extra delay before scsi_remove_host\n",
                vtl_compute_bringup_drain_ms());

    mutex_lock(&vtl_list_lock);
    list_for_each_entry(vhost, &vtl_host_list, list) {
        struct vtl_changer *ch;

        if (!vhost->scsi_registered || !vhost->shost)
            continue;
        scsi_remove_host(vhost->shost);
        vhost->scsi_registered = false;
        down_write(&vhost->io_sem);
        ch = vhost->changer;
        vhost->changer = NULL;
        smp_wmb();
        if (ch) {
            vtl_changer_clear_media(ch);
            vtl_changer_free(ch);
        }
        up_write(&vhost->io_sem);
    }
    mutex_unlock(&vtl_list_lock);

    if (for_module_exit) {
        unsigned int q;

        /*
         * Upper layers (st/ch) may still detach after scsi_remove_host; give
         * the block/SCSI stack time before platform unregister / slab teardown.
         */
        msleep(2000);
        synchronize_rcu();
        q = (unsigned int)max_t(int, 0, vtl_rmmod_quiesce_ms);
        if (q > 60000U)
            q = 60000U;
        if (q > 0)
            msleep(q);
    }
}

static int vtl_wait_all_hosts_bringup_idle(unsigned int max_ms)
{
    unsigned long deadline;

    if (!max_ms)
        return 0;

    deadline = jiffies + msecs_to_jiffies(max_ms);
    for (;;) {
        struct vtl_host *vhost;
        bool all_done = true;

        mutex_lock(&vtl_list_lock);
        list_for_each_entry(vhost, &vtl_host_list, list) {
            if (!vhost->scan_done)
                all_done = false;
        }
        mutex_unlock(&vtl_list_lock);

        if (all_done)
            return 0;
        if (time_after(jiffies, deadline))
            return -ETIMEDOUT;
        msleep(50);
    }
}

static int vtl_count_scsi_devices_on_hosts(void)
{
    struct vtl_host *vhost;
    int count = 0;

    mutex_lock(&vtl_list_lock);
    list_for_each_entry(vhost, &vtl_host_list, list) {
        struct Scsi_Host *sh = vhost->shost;
        struct scsi_device *sdev;
        unsigned long flags;

        if (!vhost->scsi_registered || !sh)
            continue;
        spin_lock_irqsave(sh->host_lock, flags);
        list_for_each_entry(sdev, &sh->__devices, siblings)
            count++;
        spin_unlock_irqrestore(sh->host_lock, flags);
    }
    mutex_unlock(&vtl_list_lock);
    return count;
}

static int vtl_rollback_geometry(const struct vtl_geom_snapshot *snap)
{
    int ret;

    vtl_restore_geometry_arrays(snap);
    ret = vtl_register_all_pdevs();
    if (ret < 0) {
        pr_crit(
            "VTL: rollback register failed err=%d — manual vtl-kernelctl reload required\n",
            ret);
        vtl_instances_kern[0] = '\0';
        vtl_instances = NULL;
        return ret;
    }
    if (snap->spec[0])
        vtl_publish_instances_spec(snap->spec);
    else
        vtl_publish_live_instances_spec();
    pr_warn("VTL: hot reconfigure rolled back to previous geometry (%s)\n",
        vtl_instances ? vtl_instances : "(built)");
    return 0;
}

static int vtl_instances_param_get(char *buf, const struct kernel_param *kp)
{
    char *cur = *(char **)kp->arg;

    if (!cur || !cur[0])
        return scnprintf(buf, PAGE_SIZE, "(null)\n");
    return scnprintf(buf, PAGE_SIZE, "%s\n", cur);
}

static int vtl_instances_param_set(const char *val,
                   const struct kernel_param *kp)
{
    char **cur = (char **)kp->arg;

    if (!val || !val[0]) {
        vtl_instances_kern[0] = '\0';
        *cur = NULL;
        return 0;
    }
    if (strlen(val) >= VTL_INST_SPEC_MAX)
        return -EINVAL;
    strlcpy(vtl_instances_kern, val, sizeof(vtl_instances_kern));
    *cur = vtl_instances_kern;
    return 0;
}

static const struct kernel_param_ops vtl_instances_param_ops = {
    .get = vtl_instances_param_get,
    .set = vtl_instances_param_set,
};

static int vtl_parse_instances_into(void)
{
    if (!vtl_instances || !vtl_instances[0]) {
        vtl_ninstances = 1;
        vtl_inst_drives[0] = vtl_num_drives;
        vtl_inst_slots[0] = vtl_num_slots;
        return 0;
    }

    return vtl_parse_spec_tokens(vtl_instances, &vtl_ninstances,
                     vtl_inst_drives, vtl_inst_slots);
}

/** True when parsed spec matches live vtl_inst_* (no platform rebuild needed). */
static bool vtl_spec_matches_live(int ninst,
				  const int drives[VTL_MAX_SCSI_INSTANCES],
				  const int slots[VTL_MAX_SCSI_INSTANCES])
{
	int i;

	if (ninst != vtl_ninstances || ninst <= 0)
		return false;
	for (i = 0; i < ninst; i++) {
		if (vtl_inst_drives[i] != drives[i] ||
		    vtl_inst_slots[i] != slots[i])
			return false;
	}
	return true;
}

static int vtl_register_all_pdevs(void)
{
    int i;
    int error;

    if (vtl_ninstances <= 0 || vtl_ninstances > VTL_MAX_SCSI_INSTANCES) {
        pr_err("VTL: invalid vtl_ninstances=%d (check vtl_instances module param)\n",
               vtl_ninstances);
        return -EINVAL;
    }

    vtl_pdev_tab = kcalloc(vtl_ninstances, sizeof(*vtl_pdev_tab), GFP_KERNEL);
    if (!vtl_pdev_tab)
        return -ENOMEM;

    for (i = 0; i < vtl_ninstances; i++) {
        struct vtl_plat_data *plat;
        struct platform_device *pdev;

        plat = kmalloc(sizeof(*plat), GFP_KERNEL);
        if (!plat) {
            error = -ENOMEM;
            goto out_destroy_pdevs;
        }
        plat->num_drives = vtl_inst_drives[i];
        plat->num_slots = vtl_inst_slots[i];

        pdev = platform_device_alloc("vtl", i);
        if (!pdev) {
            kfree(plat);
            error = -ENOMEM;
            goto out_destroy_pdevs;
        }
        pdev->dev.platform_data = plat;

        error = platform_device_add(pdev);
        if (error) {
            pr_err("VTL: platform_device_add(%d) failed\n", i);
            pdev->dev.platform_data = NULL;
            platform_device_put(pdev);
            kfree(plat);
            goto out_destroy_pdevs;
        }
        vtl_pdev_tab[i] = pdev;
    }
    return 0;

out_destroy_pdevs:
    vtl_unregister_all_hosts(false);
    return error;
}

/** Total SCSI LUNs (changer + drives) across all VTL hosts. */
static int vtl_total_scsi_luns(void)
{
    int i, n = 0;

    for (i = 0; i < vtl_ninstances; i++)
        n += vtl_inst_drives[i] + 1;
    return n;
}

/*
 * Heavy vtl_instances (many hosts and/or 8x256) must not scsi_scan all LUNs at
 * once on Kylin 4.19. Default to noscan + serial full bringup unless user insists.
 */
static void vtl_apply_conservative_bringup_tuning(void)
{
    int total_luns = vtl_total_scsi_luns();

    if (vtl_ninstances < 4 && total_luns <= 48)
        return;

    vtl_serial_full_bringup = true;

    if (!vtl_noscan) {
        vtl_noscan = true;
        pr_warn(
            "VTL: heavy geometry (%d hosts, %d SCSI LUNs total) — forced noscan=1 for insmod stability\n",
            vtl_ninstances, total_luns);
        pr_warn(
            "VTL: after insmod, scan one host at a time, e.g. for h in /sys/class/scsi_host/host*; do echo '- - -' >$h/scan if grep -q vtl $h/proc_name\n");
    }
    if (vtl_scan_async_quiesce_ms < 3000)
        vtl_scan_async_quiesce_ms = 3000;
    if (vtl_bringup_stagger_ms < 1000)
        vtl_bringup_stagger_ms = 1000;
    if (vtl_scan_host_stagger_ms < 4000)
        vtl_scan_host_stagger_ms = 4000;
}

static void vtl_unregister_all_hosts(bool release_tape_cache)
{
	int j;

	vtl_quiesce_all_hosts(release_tape_cache);

	if (vtl_pdev_tab) {
		for (j = 0; j < vtl_ninstances; j++) {
			struct platform_device *pdev = vtl_pdev_tab[j];
			struct vtl_plat_data *plat;

			if (!pdev)
				continue;
			plat = dev_get_platdata(&pdev->dev);
			platform_device_unregister(pdev);
			kfree(plat);
			vtl_pdev_tab[j] = NULL;
		}
		kfree(vtl_pdev_tab);
		vtl_pdev_tab = NULL;
		vtl_ninstances = 0;
	}

	if (release_tape_cache)
		vtl_tapes_release_all();
}

static void vtl_destroy_all_pdevs(void)
{
	vtl_unregister_all_hosts(true);
}

static struct vtl_host *vtl_host_by_instance(int instance)
{
	struct vtl_host *vhost;

	mutex_lock(&vtl_list_lock);
	list_for_each_entry(vhost, &vtl_host_list, list) {
		if (vhost->pdev && vhost->pdev->id == instance) {
			mutex_unlock(&vtl_list_lock);
			return vhost;
		}
	}
	mutex_unlock(&vtl_list_lock);
	return NULL;
}

/** Refuse shrink if media would be orphaned in drives/slots being removed. */
static int vtl_changer_prepare_shrink(struct vtl_changer *ch, int new_drives,
				      int new_slots)
{
	int i;

	if (!ch)
		return -EINVAL;

	for (i = new_drives; i < ch->num_drives; i++) {
		struct vtl_drive *d = &ch->drives[i];

		mutex_lock(&d->lock);
		if (d->loaded_tape) {
			mutex_unlock(&d->lock);
			return -EBUSY;
		}
		mutex_unlock(&d->lock);
	}

	mutex_lock(&ch->lock);
	for (i = new_slots; i < ch->num_slots; i++) {
		if (ch->slots[i].occupied) {
			mutex_unlock(&ch->lock);
			return -EBUSY;
		}
	}
	mutex_unlock(&ch->lock);
	return 0;
}

static int vtl_resize_host_geometry(int instance, int drives, int slots)
{
	struct vtl_host *vhost;
	struct vtl_changer *ch;
	struct Scsi_Host *shost;
	int ret;

	if (instance < 0 || instance >= VTL_MAX_SCSI_INSTANCES)
		return -EINVAL;
	if (drives < 1)
		drives = 1;
	if (drives > VTL_MAX_DRIVES)
		drives = VTL_MAX_DRIVES;
	if (slots < 1)
		slots = 1;
	if (slots > VTL_MAX_SLOTS)
		slots = VTL_MAX_SLOTS;

	vhost = vtl_host_by_instance(instance);
	if (!vhost || !vhost->changer)
		return -ENODEV;

	ch = vhost->changer;
	if (drives == ch->num_drives && slots == ch->num_slots)
		return 0;

	if (drives < ch->num_drives || slots < ch->num_slots) {
		ret = vtl_changer_prepare_shrink(ch, drives, slots);
		if (ret)
			return ret;
	}

	mutex_lock(&ch->lock);
	ch->num_drives = drives;
	ch->num_slots = slots;
	mutex_unlock(&ch->lock);

	shost = vhost->shost;
	if (shost)
		shost->max_lun = (unsigned int)drives + 1U;

	if (instance < vtl_ninstances) {
		vtl_inst_drives[instance] = drives;
		vtl_inst_slots[instance] = slots;
	}

	pr_info("VTL: host %d geometry resized to %dx%d (no platform rebuild)\n",
		instance, drives, slots);
	return 0;
}

int vtl_apply_geom_resize_only(const char *spec)
{
	int ret;
	int ninst;
	int drives[VTL_MAX_SCSI_INSTANCES];
	int slots[VTL_MAX_SCSI_INSTANCES];
	int i;

	if (!spec || !spec[0])
		return -EINVAL;
	if (vtl_module_is_unloading())
		return -ENODEV;

	mutex_lock(&vtl_reconfig_mutex);

	ret = vtl_parse_spec_tokens(spec, &ninst, drives, slots);
	if (ret < 0)
		goto out_unlock;

	if (ninst != vtl_ninstances || vtl_ninstances <= 0) {
		pr_warn(
			"VTL: RESIZE_GEOMETRY refused: spec has %d host(s), module has %d (add/remove library requires vtl-kernelctl reload)\n",
			ninst, vtl_ninstances);
		ret = -EINVAL;
		goto out_unlock;
	}

	if (vtl_spec_matches_live(ninst, drives, slots)) {
		vtl_publish_instances_spec(spec);
		ret = 0;
		goto out_unlock;
	}

	{
		struct vtl_geom_snapshot snap;

		vtl_save_geometry_snapshot(&snap);
		atomic_set(&vtl_reconfig_active, 1);
		synchronize_rcu();

		for (i = 0; i < ninst; i++) {
			ret = vtl_resize_host_geometry(i, drives[i], slots[i]);
			if (ret) {
				int j;

				for (j = 0; j < i; j++)
					vtl_resize_host_geometry(j, snap.drives[j],
							       snap.slots[j]);
				pr_err(
					"VTL: RESIZE_GEOMETRY failed at host %d err=%d — rolled back %d host(s)\n",
					i, ret, i);
				ret = -EIO;
				goto out_reconfig;
			}
		}

		vtl_publish_instances_spec(spec);
		vtl_publish_live_instances_spec();
		ret = 0;

out_reconfig:
		atomic_set(&vtl_reconfig_active, 0);
		synchronize_rcu();
	}

out_unlock:
	mutex_unlock(&vtl_reconfig_mutex);
	return ret;
}

static int vtl_apply_instances_spec_now(const char *spec)
{
	int ret;
	int ninst;
	int drives[VTL_MAX_SCSI_INSTANCES];
	int slots[VTL_MAX_SCSI_INSTANCES];
	int i;
	struct vtl_geom_snapshot prev;
	int ndev;

	if (!vtl_allow_hot_geom) {
		pr_warn(
			"VTL: SET_INSTANCES refused (allow_hot_geom=0); use vtl-kernelctl reload / rmmod+insmod with vtl_instances=\n");
		return -EBUSY;
	}

	mutex_lock(&vtl_reconfig_mutex);

	ret = vtl_parse_spec_tokens(spec, &ninst, drives, slots);
	if (ret < 0)
		goto out_unlock;

	if (vtl_spec_matches_live(ninst, drives, slots)) {
		vtl_publish_instances_spec(spec);
		pr_info("VTL: geometry unchanged (%s), skipping hot rebuild\n", spec);
		ret = 0;
		goto out_unlock;
	}

	vtl_save_geometry_snapshot(&prev);

	if (vtl_hotgeom_bringup_wait_ms > 0) {
		ret = vtl_wait_all_hosts_bringup_idle(
			min_t(unsigned int, (unsigned int)vtl_hotgeom_bringup_wait_ms,
			      600000U));
		if (ret == -ETIMEDOUT)
			pr_warn("VTL: hotgeom bringup wait timed out (%d ms) — proceeding\n",
				vtl_hotgeom_bringup_wait_ms);
	}

	vtl_cancel_all_host_delayed_work();

	if (vtl_hotgeom_require_no_sdevs) {
		ndev = vtl_count_scsi_devices_on_hosts();
		if (ndev > 0) {
			pr_warn(
				"VTL: SET_INSTANCES refused: %d scsi_device(s) on VTL hosts (unexport LIO / stop backup first; or hotgeom_require_no_sdevs=0)\n",
				ndev);
			ret = -EBUSY;
			goto out_unlock;
		}
	}

	atomic_set(&vtl_reconfig_active, 1);
	synchronize_rcu();

	/* Do not vtl_tapes_release_all() here — tape images on disk must survive. */
	vtl_unregister_all_hosts(false);
	synchronize_rcu();
	if (vtl_hotgeom_quiesce_ms > 0) {
		unsigned int q = (unsigned int)vtl_hotgeom_quiesce_ms;

		if (q > 30000U)
			q = 30000U;
		msleep(q);
	}

	for (i = 0; i < ninst; i++) {
		vtl_inst_drives[i] = drives[i];
		vtl_inst_slots[i] = slots[i];
	}
	vtl_ninstances = ninst;

	ret = vtl_register_all_pdevs();
	if (ret < 0) {
		pr_err(
			"VTL: hot reconfigure failed err=%d — rolling back to previous geometry\n",
			ret);
		vtl_unregister_all_hosts(false);
		synchronize_rcu();
		if (vtl_hotgeom_quiesce_ms > 0) {
			unsigned int q = (unsigned int)vtl_hotgeom_quiesce_ms;

			if (q > 30000U)
				q = 30000U;
			msleep(q);
		}
		if (prev.ninst > 0 &&
		    vtl_rollback_geometry(&prev) == 0)
			ret = -EIO;
		else
			ret = -ENODEV;
	} else {
		pr_info("VTL: geometry reapplied (%d SCSI host(s)) without module reload\n",
			vtl_ninstances);
		vtl_publish_instances_spec(spec);
		vtl_apply_conservative_bringup_tuning();
	}

	atomic_set(&vtl_reconfig_active, 0);
	synchronize_rcu();

out_unlock:
	mutex_unlock(&vtl_reconfig_mutex);
	return ret;
}

int vtl_apply_instances_spec(const char *spec)
{
	if (!spec || !spec[0])
		return -EINVAL;

	if (vtl_module_is_unloading())
		return -ENODEV;

	if (!vtl_geom_wq)
		return vtl_apply_instances_spec_now(spec);

	mutex_lock(&vtl_geom_q_mutex);
	reinit_completion(&vtl_geom_job.done);
	strlcpy(vtl_geom_job.spec, spec, sizeof(vtl_geom_job.spec));
	queue_work(vtl_geom_wq, &vtl_geom_job.work);
	wait_for_completion(&vtl_geom_job.done);
	mutex_unlock(&vtl_geom_q_mutex);
	return vtl_geom_job.ret;
}

static void vtl_geom_work_fn(struct work_struct *work)
{
	(void)work;
	vtl_geom_job.ret = vtl_apply_instances_spec_now(vtl_geom_job.spec);
	complete(&vtl_geom_job.done);
}

static struct vtl_changer *vtl_changer_alloc(int id, int num_drives, int num_slots)
{
    struct vtl_changer *ch;
    int i;

    ch = kzalloc(sizeof(*ch), GFP_KERNEL);
    if (!ch)
        return NULL;

    ch->id = id;
    snprintf(ch->name, sizeof(ch->name), "vtl-changer-%d", id);
    ch->num_drives = num_drives;
    ch->num_slots = num_slots;
    if (ch->num_drives < 1)
        ch->num_drives = 1;
    if (ch->num_drives > VTL_MAX_DRIVES)
        ch->num_drives = VTL_MAX_DRIVES;
    if (ch->num_slots < 1)
        ch->num_slots = 1;
    if (ch->num_slots > VTL_MAX_SLOTS)
        ch->num_slots = VTL_MAX_SLOTS;
    ch->num_mailslots = VTL_MAX_MAILSLOTS;
    mutex_init(&ch->lock);

    for (i = 0; i < VTL_MAX_DRIVES; i++) {
        struct vtl_drive *drv = &ch->drives[i];

        drv->id = i;
        snprintf(drv->name, sizeof(drv->name), "vtl-drive-%d", i);
        drv->block_size = VTL_DEFAULT_BLOCK_SIZE;
        drv->at_bot = true;
        mutex_init(&drv->lock);
    }

    for (i = 0; i < ch->num_slots; i++) {
        ch->slots[i].id = i;
        ch->slots[i].occupied = false;
        ch->slots[i].tape = NULL;
    }

    for (i = 0; i < ch->num_mailslots; i++) {
        ch->mailslots[i].id = i;
        ch->mailslots[i].occupied = false;
        ch->mailslots[i].tape = NULL;
    }

    return ch;
}

static void vtl_changer_free(struct vtl_changer *ch)
{
    kfree(ch);
}

static void vtl_host_scan_handler(struct work_struct *work)
{
    struct delayed_work *dw = to_delayed_work(work);
    struct vtl_host *vhost = container_of(dw, struct vtl_host, post_add_scan_work);
    struct Scsi_Host *sh = vhost->shost;

    if (vtl_serial_full_bringup)
        return;
    if (atomic_read(&vtl_module_unloading) || atomic_read(&vtl_reconfig_active))
        return;
    if (!vhost->scsi_registered || !sh)
        return;
    pr_info("VTL: deferred scsi_scan_host host_no=%d\n", sh->host_no);
    if (vtl_serial_scsi_scan)
        mutex_lock(&vtl_host_scan_mutex);
    scsi_scan_host(sh);
    /*
     * scsi_scan_host() returns while do_scan_async / st_probe may still run.
     * scsi_complete_async_scans() is not exported on Kylin 4.19 v2401 modules
     * — approximate drain with msleep (tunable via scan_async_quiesce_ms).
     */
    if (!atomic_read(&vtl_module_unloading) && !atomic_read(&vtl_reconfig_active) &&
        vtl_scan_async_quiesce_ms > 0) {
        unsigned int q = (unsigned int)vtl_scan_async_quiesce_ms;

        if (q > 30000U)
            q = 30000U;
        msleep(q);
    }
    if (vtl_serial_scsi_scan)
        mutex_unlock(&vtl_host_scan_mutex);
    vhost->scan_done = true;
}

static void vtl_host_run_scsi_scan(struct vtl_host *vhost)
{
    struct Scsi_Host *sh = vhost->shost;

    if (!sh || !vhost->scsi_registered)
        return;

    pr_info("VTL: scsi_scan_host host_no=%d (id=%u)\n", sh->host_no,
            vhost->pdev ? (unsigned int)vhost->pdev->id : 0U);
    if (vtl_serial_scsi_scan)
        mutex_lock(&vtl_host_scan_mutex);
    scsi_scan_host(sh);
    if (!atomic_read(&vtl_module_unloading) && !atomic_read(&vtl_reconfig_active) &&
        vtl_scan_async_quiesce_ms > 0) {
        unsigned int q = (unsigned int)vtl_scan_async_quiesce_ms;

        if (q > 30000U)
            q = 30000U;
        msleep(q);
    }
    if (vtl_serial_scsi_scan)
        mutex_unlock(&vtl_host_scan_mutex);
    vhost->scan_done = true;
}

static void vtl_host_bringup_handler(struct work_struct *work)
{
    struct delayed_work *dw = to_delayed_work(work);
    struct vtl_host *vhost = container_of(dw, struct vtl_host, scan_work);
    struct Scsi_Host *sh = vhost->shost;
    int err;

    if (atomic_read(&vtl_module_unloading) || atomic_read(&vtl_reconfig_active))
        return;
    if (!vhost->pdev || !sh)
        return;

    if (vtl_serial_full_bringup)
        mutex_lock(&vtl_full_bringup_mutex);

    pr_info("VTL: deferred scsi_add_host (%s)\n", dev_name(&vhost->pdev->dev));
    err = scsi_add_host(sh, &vhost->pdev->dev);
    if (err) {
        pr_err("VTL: scsi_add_host failed err=%d\n", err);
        vhost->scan_done = true;
        goto out_full_bringup;
    }
    vhost->scsi_registered = true;

    if (vtl_noscan) {
        pr_info("VTL: noscan=1 — no scsi_scan_host; host_no=%d: echo \"- - -\" > /sys/class/scsi_host/host%d/scan\n",
                sh->host_no, sh->host_no);
        vhost->scan_done = true;
        goto out_full_bringup;
    }

    if (vtl_serial_full_bringup) {
        vtl_host_run_scsi_scan(vhost);
        goto out_full_bringup;
    }

    {
        unsigned int post = (unsigned int)max_t(int, 0, vtl_post_add_scan_delay_ms);
        unsigned int ssh_st = (unsigned int)max_t(int, 0, vtl_scan_host_stagger_ms);
        unsigned int inst = (unsigned int)max_t(int, 0, vhost->pdev->id);
        u64 total_ms = (u64)post + (u64)inst * (u64)ssh_st;

        if (total_ms > (u64)UINT_MAX)
            total_ms = UINT_MAX;
        pr_info("VTL: scsi_scan_host deferred %ums (post=%u id=%u scan_stagger=%u) host_no=%d\n",
                (unsigned int)total_ms, post, inst, ssh_st, sh->host_no);
        if (vtl_bringup_wq && !vtl_module_is_unloading())
            queue_delayed_work(
                vtl_bringup_wq, &vhost->post_add_scan_work,
                total_ms ? msecs_to_jiffies((unsigned int)total_ms) : 1);
    }

out_full_bringup:
    if (vtl_serial_full_bringup)
        mutex_unlock(&vtl_full_bringup_mutex);
}

static int vtl_probe(struct platform_device *pdev)
{
    struct Scsi_Host *shost;
    struct vtl_host *vhost;
    struct vtl_changer *changer;
    struct vtl_plat_data *plat = dev_get_platdata(&pdev->dev);
    int nd, ns;
    int error;

    if (plat) {
        nd = plat->num_drives;
        ns = plat->num_slots;
    } else {
        nd = vtl_num_drives;
        ns = vtl_num_slots;
    }
    if (nd < 1 || nd > VTL_MAX_DRIVES || ns < 1 || ns > VTL_MAX_SLOTS) {
        dev_err(&pdev->dev, "invalid geometry drives=%d slots=%d\n", nd, ns);
        return -EINVAL;
    }

    shost = scsi_host_alloc(&vtl_sht, sizeof(struct vtl_host));
    if (!shost) {
        dev_err(&pdev->dev, "scsi_host_alloc failed\n");
        return -ENOMEM;
    }

    vhost = shost_priv(shost);
    vhost->shost = shost;
    vhost->pdev = pdev;
    vhost->scsi_registered = false;
    vhost->scan_done = false;
    init_rwsem(&vhost->io_sem);

    changer = vtl_changer_alloc(pdev->id, nd, ns);
    if (!changer) {
        error = -ENOMEM;
        goto out_free_host;
    }
    vhost->changer = changer;

    /*
     * LUN 0 = medium changer; LUN 1..num_drives = tape drives.
     * shost->max_lun is an exclusive upper bound (valid LUNs are 0 .. max_lun-1).
     *
     * scsi_host_alloc defaults leave max_id > 1, so the mid-layer scans target
     * IDs 1..N; this LLD would answer every INQUIRY the same, duplicating
     * changer + tape per SCSI ID. Only target 0 exists.
     */
    shost->max_lun = changer->num_drives + 1;
    shost->max_channel = 0;
    shost->max_id = 1;

    INIT_DELAYED_WORK(&vhost->scan_work, vtl_host_bringup_handler);
    INIT_DELAYED_WORK(&vhost->post_add_scan_work, vtl_host_scan_handler);
    {
        unsigned int base = (unsigned int)max_t(int, 0, vtl_scan_delay_ms);
        unsigned int stagger = (unsigned int)max_t(int, 0, vtl_bringup_stagger_ms);
        unsigned int inst = (unsigned int)max_t(int, 0, pdev->id);
        u64 total_ms = (u64)base + (u64)inst * (u64)stagger;

        if (total_ms > (u64)UINT_MAX)
            total_ms = UINT_MAX;
        if (vtl_bringup_wq)
            queue_delayed_work(
                vtl_bringup_wq, &vhost->scan_work,
                total_ms ? msecs_to_jiffies((unsigned int)total_ms) : 1);
    }

    mutex_lock(&vtl_list_lock);
    list_add_tail(&vhost->list, &vtl_host_list);
    mutex_unlock(&vtl_list_lock);

    platform_set_drvdata(pdev, vhost);

    dev_info(&pdev->dev,
             "VTL host prepared (drives=%d slots=%d); scsi_add_host deferred (base=%ums stagger=%ums id=%d post_scan=%u scan_host_stagger=%u noscan=%d)\n",
             changer->num_drives, changer->num_slots, vtl_scan_delay_ms,
             vtl_bringup_stagger_ms, pdev->id, vtl_post_add_scan_delay_ms,
             vtl_scan_host_stagger_ms, vtl_noscan ? 1 : 0);
    return 0;

out_free_host:
    scsi_host_put(shost);
    return error;
}

static int vtl_remove(struct platform_device *pdev)
{
    struct vtl_host *vhost = platform_get_drvdata(pdev);
    struct vtl_changer *changer;
    struct Scsi_Host *shost;

    if (!vhost)
        return 0;

    cancel_delayed_work_sync(&vhost->scan_work);
    cancel_delayed_work_sync(&vhost->post_add_scan_work);

    mutex_lock(&vtl_list_lock);
    list_del(&vhost->list);
    mutex_unlock(&vtl_list_lock);

    shost = vhost->shost;
    if (vhost->scsi_registered && shost) {
        scsi_remove_host(shost);
        vhost->scsi_registered = false;
    }

    down_write(&vhost->io_sem);
    changer = vhost->changer;
    vhost->changer = NULL;
    smp_wmb();
    if (changer) {
        vtl_changer_clear_media(changer);
        vtl_changer_free(changer);
    }
    up_write(&vhost->io_sem);
    if (shost)
        scsi_host_put(shost);

    return 0;
}

static struct platform_driver vtl_platform_driver = {
    .driver = {
        .name = "vtl",
        .owner = THIS_MODULE,
    },
    .probe = vtl_probe,
    .remove = vtl_remove,
};

static int __init vtl_init(void)
{
    int error;

    pr_info("VTL: Loading Virtual Tape Library v%s (kernel %d.%d.%d)\n",
            VTL_VERSION,
            (int)LINUX_VERSION_MAJOR, (int)LINUX_VERSION_PATCHLEVEL,
            (int)LINUX_VERSION_SUBLEVEL);

    error = vtl_parse_instances_into();
    if (error)
        return error;

    vtl_personality_set_active(vtl_personality_resolve_name(personality));
    pr_info("VTL: personality=%s (INQUIRY vendor %s)\n",
            vtl_personality_lookup(vtl_personality_active_id())->name,
            vtl_personality_lookup(vtl_personality_active_id())->vendor);

    vtl_geom_wq = alloc_workqueue("vtl_geom", WQ_MEM_RECLAIM, 1);
    if (!vtl_geom_wq) {
        pr_err("VTL: alloc_workqueue(vtl_geom) failed\n");
        return -ENOMEM;
    }
    INIT_WORK(&vtl_geom_job.work, vtl_geom_work_fn);
    init_completion(&vtl_geom_job.done);

    vtl_bringup_wq = alloc_workqueue("vtl_bringup", WQ_MEM_RECLAIM | WQ_UNBOUND, 1);
    if (!vtl_bringup_wq) {
        pr_err("VTL: alloc_workqueue(vtl_bringup) failed\n");
        error = -ENOMEM;
        goto out_destroy_geom_wq;
    }

    atomic_set(&vtl_module_unloading, 0);
    atomic_set(&vtl_reconfig_active, 0);

    error = platform_driver_register(&vtl_platform_driver);
    if (error) {
        pr_err("VTL: platform_driver_register failed\n");
        goto out_destroy_geom_wq;
    }

    error = vtl_register_all_pdevs();
    if (error) {
        pr_err("VTL: platform device setup failed\n");
        goto out_unregister_driver;
    }
    vtl_publish_live_instances_spec();
    vtl_apply_conservative_bringup_tuning();

    error = vtl_sysfs_init();
    if (error) {
        pr_err("VTL: vtl_sysfs_init failed\n");
        goto out_destroy_pdevs;
    }

    error = vtl_misc_init();
    if (error) {
        pr_err("VTL: vtl_misc_init failed\n");
        goto out_sysfs_exit;
    }

    pr_info("VTL: Module loaded successfully (%d SCSI host(s))\n", vtl_ninstances);
    return 0;

out_sysfs_exit:
    vtl_sysfs_exit();
out_destroy_pdevs:
    vtl_destroy_all_pdevs();
out_unregister_driver:
    platform_driver_unregister(&vtl_platform_driver);
out_destroy_geom_wq:
    if (vtl_bringup_wq) {
        destroy_workqueue(vtl_bringup_wq);
        vtl_bringup_wq = NULL;
    }
    if (vtl_geom_wq) {
        destroy_workqueue(vtl_geom_wq);
        vtl_geom_wq = NULL;
    }
    return error;
}

static void __exit vtl_exit(void)
{
    pr_info("VTL: Unloading module\n");

    vtl_misc_exit();
    if (vtl_geom_wq)
        flush_workqueue(vtl_geom_wq);
    vtl_sysfs_exit();
    vtl_destroy_all_pdevs();
    platform_driver_unregister(&vtl_platform_driver);
    if (vtl_bringup_wq) {
        destroy_workqueue(vtl_bringup_wq);
        vtl_bringup_wq = NULL;
    }
    if (vtl_geom_wq) {
        destroy_workqueue(vtl_geom_wq);
        vtl_geom_wq = NULL;
    }
    atomic_set(&vtl_module_unloading, 0);
    atomic_set(&vtl_reconfig_active, 0);

    pr_info("VTL: Module unloaded\n");
}

module_init(vtl_init);
module_exit(vtl_exit);

module_param(personality, charp, 0644);
MODULE_PARM_DESC(personality,
    "SCSI INQUIRY personality: vtl (default), ibm (TS3584/LTO8), stk (L700/T10000), hp (MSL6480). Aliases: 3584, l700, msl.");

module_param_named(num_drives, vtl_num_drives, int, 0444);
module_param_named(num_slots, vtl_num_slots, int, 0444);
module_param_cb(vtl_instances, &vtl_instances_param_ops, &vtl_instances, 0444);
MODULE_PARM_DESC(num_drives,
    "Single-host mode (no vtl_instances): tape drive count (1..VTL_MAX_DRIVES). Ignored when vtl_instances is set.");
MODULE_PARM_DESC(num_slots,
    "Single-host mode (no vtl_instances): storage slots. Ignored when vtl_instances is set.");
MODULE_PARM_DESC(vtl_instances,
    "Comma-separated NxM per virtual library (N=drives, M=data slots), e.g. vtl_instances=2x64,1x10 for two SCSI hosts (max 8 libraries; N<=8 drives, M<=256 slots). When unset, num_drives/num_slots define one host. Updated after successful SET_INSTANCES or RESIZE_GEOMETRY ioctl.");
MODULE_PARM_DESC(scan_delay_ms,
    "Base milliseconds before deferred scsi_add_host on system_long_wq (default 500; 0 = one jiffy).");
MODULE_PARM_DESC(post_add_scan_delay_ms,
    "Base milliseconds after scsi_add_host before scsi_scan_host (default 600; 0 = one jiffy; ignored if noscan). Actual delay adds id*scan_host_stagger_ms.");
MODULE_PARM_DESC(bringup_stagger_ms,
    "Extra delay per host index (vtl_instances order): add_host at scan_delay_ms + id*bringup_stagger_ms (default 400; 0 = all hosts at once).");
MODULE_PARM_DESC(scan_host_stagger_ms,
    "After add_host, scsi_scan_host waits post_add_scan_delay_ms + id*scan_host_stagger_ms (default 3000; 0 = no extra per-host scan spacing). Reduces parallel st_probe on Kylin 4.19.");
MODULE_PARM_DESC(serial_scsi_scan,
    "If Y (default), scsi_scan_host is serialized across VTL hosts (mutex). Pair with scan_async_quiesce_ms so the mutex also covers post-scan sysfs/st_probe work on Kylin 4.19; set N for legacy parallel scans (not recommended on affected kernels).");
MODULE_PARM_DESC(serial_full_bringup,
    "If Y (default), each host completes scsi_add_host + scsi_scan_host + quiesce before the next starts. Auto-enabled for 4+ hosts; required for stable insmod of many 16x256 libraries on Kylin 4.19.");
MODULE_PARM_DESC(scan_async_quiesce_ms,
    "Milliseconds to sleep after each scsi_scan_host (0..30000, default 1000; 0 = off). Kylin 4.19 does not export scsi_complete_async_scans to modules; this sleep approximates async scan drain before the next host when serial_scsi_scan=Y.");
MODULE_PARM_DESC(hotgeom_quiesce_ms,
    "After SET_INSTANCES destroys old platform devices, sleep this many ms (0..30000, default 200) before re-registering; also calls synchronize_rcu(). Mitigates Kylin 4.19 kernfs/slab issues when hot-rebuilding vtl.N.");
MODULE_PARM_DESC(allow_hot_geom,
    "If Y, /dev/vtl SET_INSTANCES may rebuild SCSI hosts without rmmod (risky on Kylin 4.19 with LIO/backup I/O). Default N: use insmod vtl_instances= or vtl-kernelctl reload.");
MODULE_PARM_DESC(hotgeom_require_no_sdevs,
    "When allow_hot_geom=Y, refuse SET_INSTANCES while any scsi_device exists on VTL hosts (default Y).");
MODULE_PARM_DESC(hotgeom_bringup_wait_ms,
    "Before hot SET_INSTANCES, wait up to this many ms for deferred scsi_add_host/scan to finish (default 120000; 0=skip).");
MODULE_PARM_DESC(rmmod_quiesce_ms,
    "After scsi_remove_host on all VTL hosts during rmmod, sleep this many ms (0..60000, default 12000) before platform_device_unregister.");
MODULE_PARM_DESC(noscan,
    "If Y/1, deferred scsi_add_host runs but scsi_scan_host is skipped (then echo \"- - -\" > /sys/class/scsi_host/hostN/scan manually).");

MODULE_LICENSE("GPL");
MODULE_AUTHOR("VTL Team");
MODULE_DESCRIPTION("Virtual Tape Library");
MODULE_VERSION(VTL_VERSION);

/* Optional delay (ms) after each successful medium move — training / pacing */
int vtl_move_delay_ms;
module_param_named(move_delay_ms, vtl_move_delay_ms, int, 0644);
MODULE_PARM_DESC(move_delay_ms,
    "Sleep this many milliseconds after each successful MOVE MEDIUM (0 = off, max ~60s)");

char *vtl_tape_dir = "/opt/vtladm/var/tapes";
module_param_named(tape_dir, vtl_tape_dir, charp, 0644);
MODULE_PARM_DESC(tape_dir,
    "Directory for tape image files (<dir>/<name>.vtltape); align with vtladm tape_dir");
