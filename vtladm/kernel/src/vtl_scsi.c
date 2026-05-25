#include "../include/vtl.h"
#include "../include/vtl_personality.h"

/*
 * Big-endian helpers — avoid <linux/unaligned.h>: some vendor kernel-devel
 * packages (e.g. certain Kylin trees) omit that header while still on 4.19.
 */
static inline u32 vtl_get_be32(const u8 *p)
{
    return ((u32)p[0] << 24) | ((u32)p[1] << 16) | ((u32)p[2] << 8) | (u32)p[3];
}

static inline void vtl_put_be32(u32 v, u8 *p)
{
    p[0] = (u8)(v >> 24);
    p[1] = (u8)(v >> 16);
    p[2] = (u8)(v >> 8);
    p[3] = (u8)v;
}

static inline void vtl_put_be16(u16 v, u8 *p)
{
    p[0] = (u8)(v >> 8);
    p[1] = (u8)v;
}

static inline void vtl_put_be64(u64 v, u8 *p)
{
    p[0] = (u8)(v >> 56);
    p[1] = (u8)(v >> 48);
    p[2] = (u8)(v >> 40);
    p[3] = (u8)(v >> 32);
    p[4] = (u8)(v >> 24);
    p[5] = (u8)(v >> 16);
    p[6] = (u8)(v >> 8);
    p[7] = (u8)v;
}

/* SCSI opcodes (avoid header drift across 4.18–6.x) */
#ifndef LOG_SENSE
#define LOG_SENSE 0x4d
#endif
#ifndef READ_POSITION
#define READ_POSITION 0x34
#endif
#ifndef PREVENT_ALLOW_MEDIUM_REMOVAL
#define PREVENT_ALLOW_MEDIUM_REMOVAL 0x1e
#endif
#ifndef READ_12
#define READ_12 0xa8
#endif
#ifndef WRITE_12
#define WRITE_12 0xaa
#endif
/* SSC tape opcodes (not always in scsi.h for out-of-tree / older trees) */
#ifndef REWIND
#define REWIND 0x01
#endif
#ifndef LOAD_UNLOAD
#define LOAD_UNLOAD 0x1b
#endif
#ifndef REPORT_LUNS
#define REPORT_LUNS 0xa0
#endif
#ifndef INITIALIZE_ELEMENT_STATUS
#define INITIALIZE_ELEMENT_STATUS 0x07
#endif
#ifndef DRIVER_SENSE
#define DRIVER_SENSE 0x08
#endif

/*
 * Compose cmd->result for SG_IO / sg3_utils: status in bits 0..7 is SAM status << 1;
 * CHECK CONDITION also needs DRIVER_SENSE in bits 8..15. Raw SAM_STAT_* alone breaks
 * sg_turs ("bad pass-through setup") even when the command logic is correct.
 */
static void vtl_set_cmd_result(struct scsi_cmnd *cmd, int sam_status)
{
    cmd->result = DID_OK << 16;
    if (sam_status == SAM_STAT_CHECK_CONDITION)
        cmd->result |= (DRIVER_SENSE << 8) | (SAM_STAT_CHECK_CONDITION << 1);
    else if (sam_status != SAM_STAT_GOOD)
        cmd->result |= (sam_status << 1);
}

/* Max single READ/WRITE buffer (matches VTL_SCSI_RW_CAP_BYTES below) */
#define VTL_XFER_BUF_MAX (64U * 1024U * 1024U)
#define VTL_ELEMENT_STATUS_BUFLEN 8192U

static void *vtl_xfer_buf_alloc(unsigned int len)
{
    if (len == 0 || len > VTL_XFER_BUF_MAX)
        return NULL;
    return kmalloc(len, GFP_KERNEL | __GFP_NOWARN);
}

static void vtl_xfer_buf_free(void *p)
{
    kfree(p);
}

void vtl_set_sense(struct vtl_sense_data *sense, u8 key, u8 asc, u8 ascq)
{
    sense->key = key;
    sense->asc = asc;
    sense->ascq = ascq;
}

void vtl_build_sense_buffer(struct scsi_cmnd *cmd, struct vtl_sense_data *sense)
{
    u8 *sb = cmd->sense_buffer;

    memset(sb, 0, SCSI_SENSE_BUFFERSIZE);
    sb[0] = 0x70;
    sb[2] = sense->key;
    sb[7] = 10;
    sb[12] = sense->asc;
    sb[13] = sense->ascq;
}

#ifndef ABORTED_COMMAND
#define ABORTED_COMMAND 0x0b
#endif
#ifndef HARDWARE_ERROR
#define HARDWARE_ERROR 0x04
#endif
#ifndef VOLUME_OVERFLOW
#define VOLUME_OVERFLOW 0x0d
#endif

static bool vtl_drive_has_tape(struct vtl_drive *drv)
{
    bool loaded;

    mutex_lock(&drv->lock);
    loaded = drv->loaded_tape != NULL;
    mutex_unlock(&drv->lock);
    return loaded;
}

static bool vtl_drive_write_protected(struct vtl_drive *drv)
{
    bool wp;

    mutex_lock(&drv->lock);
    wp = drv->loaded_tape && drv->loaded_tape->write_protected;
    mutex_unlock(&drv->lock);
    return wp;
}

/* Linear staging buffer → initiator scatter-gather (returns 0 or -EIO). */
static int vtl_scsi_copy_to_sg(struct scsi_cmnd *cmd, void *buf, unsigned int len,
                               struct vtl_sense_data *sense)
{
    int copied;

    if (len == 0)
        return 0;

    copied = scsi_sg_copy_from_buffer(cmd, buf, (int)len);
    if (unlikely(copied < 0)) {
        vtl_set_sense(sense, ABORTED_COMMAND, 0x00, 0x00);
        vtl_build_sense_buffer(cmd, sense);
        return -EIO;
    }
    if (unlikely((unsigned int)copied < len))
        scsi_set_resid(cmd, len - (unsigned int)copied);
    return 0;
}

/* Initiator scatter-gather → linear staging buffer (returns 0 or -EIO). */
static int vtl_scsi_copy_from_sg(struct scsi_cmnd *cmd, void *buf, unsigned int len,
                                 struct vtl_sense_data *sense)
{
    int copied;

    if (len == 0)
        return 0;

    copied = scsi_sg_copy_to_buffer(cmd, buf, (int)len);
    if (unlikely(copied < 0)) {
        vtl_set_sense(sense, ABORTED_COMMAND, 0x00, 0x00);
        vtl_build_sense_buffer(cmd, sense);
        return -EIO;
    }
    if (unlikely((unsigned int)copied < len)) {
        vtl_set_sense(sense, ABORTED_COMMAND, 0x00, 0x00);
        vtl_build_sense_buffer(cmd, sense);
        return -EIO;
    }
    return 0;
}

static void vtl_scsi_staging_oom(struct scsi_cmnd *cmd, struct vtl_sense_data *sense)
{
    vtl_set_sense(sense, HARDWARE_ERROR, 0x00, 0x00);
    vtl_build_sense_buffer(cmd, sense);
}

static int vtl_cmd_illegal(struct scsi_cmnd *cmd, struct vtl_sense_data *sense)
{
    vtl_set_sense(sense, ILLEGAL_REQUEST, 0x20, 0);
    vtl_build_sense_buffer(cmd, sense);
    return SAM_STAT_CHECK_CONDITION;
}

/* ILLEGAL REQUEST / ASC 0x25 ASCQ 0x00 — logical unit not supported */
static int vtl_cmd_lun_not_supported(struct scsi_cmnd *cmd, struct vtl_changer *ch)
{
    vtl_set_sense(&ch->sense, ILLEGAL_REQUEST, 0x25, 0);
    vtl_build_sense_buffer(cmd, &ch->sense);
    return SAM_STAT_CHECK_CONDITION;
}

static struct vtl_sense_data *vtl_sense_ptr(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    unsigned int lun = cmd->device->lun;
    struct vtl_changer *ch = vhost->changer;

    if (lun == 0)
        return &ch->sense;
    if (lun >= 1 && lun <= (unsigned int)ch->num_drives)
        return &ch->drives[lun - 1].sense;
    return &ch->sense;
}

static unsigned int vtl_inquiry_alloc_len(struct scsi_cmnd *cmd)
{
    const u8 *cdb = cmd->cmnd;

    if (cmd->cmd_len >= 10)
        return ((unsigned int)cdb[3] << 8) | cdb[4];
    return (unsigned int)cdb[4];
}

static int vtl_handle_inquiry_evpd(struct scsi_cmnd *cmd, struct vtl_host *vhost, u8 page)
{
    u8 *buf;
    unsigned int lun = cmd->device->lun;
    unsigned int alloc = vtl_inquiry_alloc_len(cmd);
    unsigned int out_len;
    unsigned int buflen = 512;
    u8 ptype = (lun == 0) ? 0x08 : 0x01;
    char id8[9];

    buf = vtl_xfer_buf_alloc(buflen);
    if (!buf) {
        vtl_scsi_staging_oom(cmd, vtl_sense_ptr(cmd, vhost));
        return SAM_STAT_CHECK_CONDITION;
    }

    memset(buf, 0, buflen);
    buf[0] = ptype;
    buf[1] = page;

    switch (page) {
    case 0x00:
        buf[3] = 3;
        buf[4] = 0x00;
        buf[5] = 0x80;
        buf[6] = 0x83;
        out_len = 7;
        break;
    case 0x80: {
        /*
         * Unit Serial Number (IBM/TSM/Mars 清单常探测；缺此页会 ILLEGAL REQUEST)。
         */
        struct Scsi_Host *shost = vhost->shost;
        char serial[32];

        snprintf(serial, sizeof(serial), "VTL%05uL%02u",
                 shost ? (unsigned int)shost->host_no : 0U, lun);
        out_len = 4 + (unsigned int)strnlen(serial, 20U);
        if (out_len > buflen)
            out_len = buflen;
        buf[3] = (u8)(out_len - 4);
        memcpy(&buf[4], serial, out_len - 4);
        break;
    }
    case 0x83: {
        /*
         * Device Identification VPD: one vendor-specific descriptor (SPC).
         * 8-byte identifier encodes LUN so multipath tools can distinguish LUNs.
         */
        buf[3] = 12;
        buf[4] = 0x01; /* PI=0, code set = binary */
        buf[5] = 0x00; /* designator type 0 = vendor-specific */
        buf[6] = 0x00;
        buf[7] = 8;
        snprintf(id8, sizeof(id8), "VTL%05u", lun);
        memcpy(&buf[8], id8, 8);
        out_len = 4 + 12;
        break;
    }
    default:
        vtl_xfer_buf_free(buf);
        return vtl_cmd_illegal(cmd, vtl_sense_ptr(cmd, vhost));
    }

    alloc = min_t(unsigned int, alloc, buflen);
    if (vtl_scsi_copy_to_sg(cmd, buf, min_t(unsigned int, alloc, out_len), vtl_sense_ptr(cmd, vhost))) {
        vtl_xfer_buf_free(buf);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buf);
    return SAM_STAT_GOOD;
}

static int vtl_handle_inquiry(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    u8 *cdb = cmd->cmnd;
    u8 *buffer;
    unsigned int len;
    unsigned int alloc;
    unsigned int lun = cmd->device->lun;

    if (cdb[1] & 0x02)
        return vtl_cmd_illegal(cmd, vtl_sense_ptr(cmd, vhost));
    if (cdb[1] & 0x01)
        return vtl_handle_inquiry_evpd(cmd, vhost, cdb[2]);

    buffer = vtl_xfer_buf_alloc(252);
    if (!buffer) {
        vtl_scsi_staging_oom(cmd, vtl_sense_ptr(cmd, vhost));
        return SAM_STAT_CHECK_CONDITION;
    }

    memset(buffer, 0, 252);
    /* Peripheral qualifier 000b, device type */
    if (lun == 0)
        buffer[0] = 0x08; /* Medium changer */
    else
        buffer[0] = 0x01; /* Sequential-access */
    buffer[1] = 0x80; /* Removable */
    buffer[2] = 2;
    buffer[3] = 2;
    buffer[4] = 31;
    buffer[5] = 0;
    buffer[6] = 0;
    buffer[7] = 0;
    {
        const struct vtl_personality_desc *pers =
            vtl_personality_lookup(vtl_personality_active_id());

        memcpy(&buffer[8], pers->vendor, 8);
        if (lun == 0)
            memcpy(&buffer[16], pers->product_changer, 16);
        else
            memcpy(&buffer[16], pers->product_tape, 16);
        memcpy(&buffer[32], pers->revision, 4);
    }

    alloc = vtl_inquiry_alloc_len(cmd);
    len = min_t(unsigned int, alloc, 252U);
    if (vtl_scsi_copy_to_sg(cmd, buffer, len, vtl_sense_ptr(cmd, vhost))) {
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buffer);

    return SAM_STAT_GOOD;
}

static int vtl_handle_test_unit_ready(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    unsigned int lun = cmd->device->lun;
    struct vtl_changer *ch = vhost->changer;
    struct vtl_drive *drv;

    if (lun == 0)
        return SAM_STAT_GOOD;

    if (lun < 1 || lun > (unsigned int)ch->num_drives)
        return vtl_cmd_lun_not_supported(cmd, ch);

    drv = &ch->drives[lun - 1];
    /*
     * Empty drive: return GOOD so backup inventory (Mars/TSM) can enumerate
     * the library + drives. Medium-not-present (0x3a) is reported on READ/
     * LOAD, not on TUR — mtx/changer inventory only needs the drive element.
     */
    return SAM_STAT_GOOD;
}

static int vtl_handle_request_sense(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    u8 *cdb = cmd->cmnd;
    u8 *buffer;
    int len;
    struct vtl_sense_data *src;

    buffer = vtl_xfer_buf_alloc(252);
    if (!buffer) {
        vtl_scsi_staging_oom(cmd, vtl_sense_ptr(cmd, vhost));
        return SAM_STAT_CHECK_CONDITION;
    }

    src = vtl_sense_ptr(cmd, vhost);
    vtl_build_sense_buffer(cmd, src);
    len = min_t(int, cdb[4] ? cdb[4] : 252, 252);
    len = min_t(int, len, 96);
    memcpy(buffer, cmd->sense_buffer, len);
    if (vtl_scsi_copy_to_sg(cmd, buffer, (unsigned int)len, vtl_sense_ptr(cmd, vhost))) {
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buffer);

    return SAM_STAT_GOOD;
}

static int vtl_handle_read_block_limits(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    u8 *buffer;

    buffer = vtl_xfer_buf_alloc(6);
    if (!buffer) {
        vtl_scsi_staging_oom(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    memset(buffer, 0, 6);
    buffer[1] = (VTL_MAX_BLOCK_SIZE >> 16) & 0xff;
    buffer[2] = (VTL_MAX_BLOCK_SIZE >> 8) & 0xff;
    buffer[3] = VTL_MAX_BLOCK_SIZE & 0xff;
    buffer[4] = (VTL_MIN_BLOCK_SIZE >> 8) & 0xff;
    buffer[5] = (VTL_MIN_BLOCK_SIZE >> 0) & 0xff;

    if (vtl_scsi_copy_to_sg(cmd, buffer, 6, &drv->sense)) {
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buffer);

    return SAM_STAT_GOOD;
}

/*
 * Medium-changer MODE SENSE pages (mtx / backup apps probe 0x1D / 0x1E).
 * MODE SENSE(6): 4-byte header + page; MODE SENSE(10): 8-byte header + page.
 */
static unsigned int vtl_changer_mode_sense_fill(u8 *buffer, unsigned int buf_max,
						struct vtl_changer *ch, u8 page,
						bool sense10)
{
    unsigned int hdr = sense10 ? 8U : 4U;
    unsigned int off = hdr;
    u8 *pg;

    if (buf_max < hdr + 4)
        return hdr;

    memset(buffer, 0, buf_max);

    if (page == 0x00) {
        if (off + 6 > buf_max)
            return hdr;
        pg = &buffer[off];
        pg[0] = 0x00;
        pg[1] = 4;
        pg[2] = 0x00;
        pg[3] = 0x01;
        pg[4] = 0x1d;
        pg[5] = 0x1e;
        off += 6;
    } else if (page == 0x1d) {
        /*
         * SMC-3 Element Address Assignment (page 0x1D), 16-byte parameter list:
         *  2-3  first medium transport, 4-5  #MT
         *  6-7  first storage,         8-9  #storage
         * 10-11 first I/E,            12-13 #I/E
         * 14-15 first data transfer,  16-17 #drives
         * (Legacy code wrongly put #slots at 4-5 and drive base at 6-7, so
         * initiators saw only two storage elements at 1000-1001.)
         */
        if (off + 18 > buf_max)
            return hdr;
        pg = &buffer[off];
        pg[0] = 0x1d;
        pg[1] = 16;
        vtl_put_be16(0, &pg[2]);
        vtl_put_be16(0, &pg[4]);
        vtl_put_be16(0, &pg[6]);
        vtl_put_be16(ch->num_slots, &pg[8]);
        vtl_put_be16(VTL_ELEM_IE_BASE, &pg[10]);
        vtl_put_be16(ch->num_mailslots, &pg[12]);
        vtl_put_be16(VTL_ELEM_DRIVE_BASE, &pg[14]);
        vtl_put_be16(ch->num_drives, &pg[16]);
        off += 18;
    } else if (page == 0x1e) {
        if (off + 8 > buf_max)
            return hdr;
        pg = &buffer[off];
        pg[0] = 0x1e;
        pg[1] = 6;
        pg[2] = 1;
        pg[3] = 0;
        pg[4] = 0;
        pg[5] = 0;
        pg[6] = 0;
        pg[7] = 0;
        off += 8;
    } else if (page == 0x3f) {
        /*
         * All mode pages: backup apps (Mars/TSM) often use 0x3F instead of
         * separate 0x1D probes; returning header-only broke inventory while mtx
         * still worked (mtx requests page 0x1D explicitly).
         */
        if (off + 18 > buf_max)
            return hdr;
        pg = &buffer[off];
        pg[0] = 0x1d;
        pg[1] = 16;
        vtl_put_be16(0, &pg[2]);
        vtl_put_be16(0, &pg[4]);
        vtl_put_be16(0, &pg[6]);
        vtl_put_be16(ch->num_slots, &pg[8]);
        vtl_put_be16(VTL_ELEM_IE_BASE, &pg[10]);
        vtl_put_be16(ch->num_mailslots, &pg[12]);
        vtl_put_be16(VTL_ELEM_DRIVE_BASE, &pg[14]);
        vtl_put_be16(ch->num_drives, &pg[16]);
        off += 18;
        if (off + 8 <= buf_max) {
            pg = &buffer[off];
            pg[0] = 0x1e;
            pg[1] = 6;
            pg[2] = 1;
            pg[3] = 0;
            pg[4] = 0;
            pg[5] = 0;
            pg[6] = 0;
            pg[7] = 0;
            off += 8;
        }
    }

    if (sense10) {
        /*
         * MODE SENSE(10): bytes 0-1 = length of bytes 2..(n-1) per SAM-5.
         * (Legacy used plen+2 with plen=off-8, short by 4 bytes — initiators
         * truncated page 0x1D before drive base @1000 / #drives; backup inventory failed.)
         */
        unsigned int md_len = off - 2;

        buffer[0] = (md_len >> 8) & 0xff;
        buffer[1] = md_len & 0xff;
        buffer[2] = 0;
        buffer[3] = 0;
        return off;
    }

    buffer[0] = (off - 1) & 0xff;
    buffer[1] = 0;
    buffer[2] = 0;
    buffer[3] = 0;
    return off;
}

static int vtl_handle_mode_sense(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    u8 *cdb = cmd->cmnd;
    u8 *buffer;
    unsigned int alloc_len;
    unsigned int lun = cmd->device->lun;
    struct vtl_changer *ch = vhost->changer;
    struct vtl_drive *drv = NULL;
    u32 block_len;
    u8 wp;
    unsigned int out_len;
    u8 page;

    if (lun >= 1 && lun <= (unsigned int)ch->num_drives)
        drv = &ch->drives[lun - 1];

    if (cdb[0] == MODE_SENSE)
        alloc_len = cdb[4];
    else
        alloc_len = (cdb[7] << 8) | cdb[8];

    buffer = vtl_xfer_buf_alloc(255);
    if (!buffer) {
        vtl_scsi_staging_oom(cmd, vtl_sense_ptr(cmd, vhost));
        return SAM_STAT_CHECK_CONDITION;
    }

    memset(buffer, 0, 255);

    if (lun == 0) {
        page = cdb[2] & 0x3f;
        out_len = vtl_changer_mode_sense_fill(buffer, 255, ch, page,
					    cdb[0] == MODE_SENSE_10);
    } else if (cdb[0] == MODE_SENSE) {
        /* MODE SENSE(6): SSC block descriptor so st/mt see non-zero block size */
        block_len = drv ? drv->block_size : VTL_DEFAULT_BLOCK_SIZE;
        wp = (drv && vtl_drive_write_protected(drv)) ? 0x80 : 0;

        buffer[0] = 11;
        buffer[1] = 0;
        buffer[2] = wp;
        buffer[3] = 8;
        buffer[4] = 0;
        buffer[5] = 0;
        buffer[6] = 0;
        buffer[7] = 0;
        buffer[8] = 0;
        buffer[9] = (block_len >> 16) & 0xff;
        buffer[10] = (block_len >> 8) & 0xff;
        buffer[11] = (block_len >> 0) & 0xff;
        out_len = 12;
    } else {
        /* MODE SENSE(10): 8-byte mode param header + 8-byte block descriptor */
        block_len = drv ? drv->block_size : VTL_DEFAULT_BLOCK_SIZE;
        wp = (drv && vtl_drive_write_protected(drv)) ? 0x80 : 0;

        buffer[0] = 0;
        buffer[1] = 14;
        buffer[2] = 0;
        buffer[3] = wp;
        buffer[4] = 0;
        buffer[5] = 0;
        buffer[6] = 8;
        buffer[7] = 0;
        buffer[8] = 0;
        buffer[9] = 0;
        buffer[10] = 0;
        buffer[11] = 0;
        buffer[12] = 0;
        buffer[13] = (block_len >> 16) & 0xff;
        buffer[14] = (block_len >> 8) & 0xff;
        buffer[15] = (block_len >> 0) & 0xff;
        out_len = 16;
    }

    alloc_len = min_t(unsigned int, alloc_len, 255U);
    if (vtl_scsi_copy_to_sg(cmd, buffer, min_t(unsigned int, alloc_len, out_len), vtl_sense_ptr(cmd, vhost))) {
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buffer);

    return SAM_STAT_GOOD;
}

static int vtl_handle_mode_select(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    return SAM_STAT_GOOD;
}

static void vtl_parse_rw_blocks(const u8 *cdb, u8 op, u32 *blocks, u32 *block_len,
                                  struct vtl_drive *drv)
{
    *block_len = drv->block_size;
    *blocks = 0;

    switch (op) {
    case READ_6:
    case WRITE_6:
        *blocks = ((u32)cdb[2] << 16) | ((u32)cdb[3] << 8) | (u32)cdb[4];
        break;
    case READ_10:
    case WRITE_10:
        *blocks = (cdb[7] << 8) | cdb[8];
        if (cdb[1] & 0x02) {
            *block_len = (cdb[6] << 16) | (cdb[7] << 8) | cdb[8];
            *blocks = 1;
        }
        break;
    case READ_12:
    case WRITE_12:
        *blocks = vtl_get_be32(&cdb[6]);
        if (cdb[1] & 0x02) {
            *block_len = vtl_get_be32(&cdb[6]);
            *blocks = 1;
        }
        break;
    default:
        break;
    }
}

static int vtl_get_s24(const u8 *p)
{
    u32 v = ((u32)p[0] << 16) | ((u32)p[1] << 8) | (u32)p[2];

    if (v & 0x00800000)
        v |= 0xff000000;
    return (s32)v;
}

static u32 vtl_get_u24(const u8 *p)
{
    return ((u32)p[0] << 16) | ((u32)p[1] << 8) | (u32)p[2];
}

/* Single-command READ/WRITE byte ceiling (avoids overflow and abusive CDB values) */
#define VTL_SCSI_RW_CAP_BYTES VTL_XFER_BUF_MAX

static int vtl_rw_prepare_xfer(struct scsi_cmnd *cmd, u32 *blocks, u32 *block_len,
                               struct vtl_sense_data *sense)
{
    u64 bytes;

    if (*blocks == 0 || *block_len == 0) {
        vtl_set_sense(sense, ILLEGAL_REQUEST, 0x24, 0);
        vtl_build_sense_buffer(cmd, sense);
        return -EINVAL;
    }
    if (*block_len > VTL_MAX_BLOCK_SIZE || *block_len < VTL_MIN_BLOCK_SIZE) {
        vtl_set_sense(sense, ILLEGAL_REQUEST, 0x24, 0);
        vtl_build_sense_buffer(cmd, sense);
        return -EINVAL;
    }

    bytes = (u64)(*blocks) * (u64)(*block_len);
    if (bytes > (u64)VTL_SCSI_RW_CAP_BYTES) {
        vtl_set_sense(sense, ILLEGAL_REQUEST, 0x24, 0);
        vtl_build_sense_buffer(cmd, sense);
        return -EINVAL;
    }
    if (bytes > 0xffffffffULL) {
        vtl_set_sense(sense, ILLEGAL_REQUEST, 0x24, 0);
        vtl_build_sense_buffer(cmd, sense);
        return -EINVAL;
    }

    return 0;
}

static int vtl_handle_read(struct scsi_cmnd *cmd, struct vtl_drive *drv, u8 op)
{
    u8 *cdb = cmd->cmnd;
    u8 *buffer;
    u32 block_len;
    u32 blocks;
    u32 actual;
    int ret;

    vtl_parse_rw_blocks(cdb, op, &blocks, &block_len, drv);
    if (vtl_rw_prepare_xfer(cmd, &blocks, &block_len, &drv->sense) < 0)
        return SAM_STAT_CHECK_CONDITION;

    buffer = vtl_xfer_buf_alloc(blocks * block_len);
    if (!buffer) {
        vtl_scsi_staging_oom(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    ret = vtl_tape_read(drv, buffer, blocks * block_len, &actual);
    if (ret < 0) {
        if (ret == -ENODEV)
            vtl_set_sense(&drv->sense, NOT_READY, 0x3a, 0);
        else
            vtl_set_sense(&drv->sense, MEDIUM_ERROR, 0x11, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }

    if (vtl_scsi_copy_to_sg(cmd, buffer, actual, &drv->sense)) {
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buffer);

    return SAM_STAT_GOOD;
}

static int vtl_handle_write(struct scsi_cmnd *cmd, struct vtl_drive *drv, u8 op)
{
    u8 *cdb = cmd->cmnd;
    u8 *buffer;
    u32 block_len;
    u32 blocks;
    int ret;

    vtl_parse_rw_blocks(cdb, op, &blocks, &block_len, drv);
    if (vtl_rw_prepare_xfer(cmd, &blocks, &block_len, &drv->sense) < 0)
        return SAM_STAT_CHECK_CONDITION;

    buffer = vtl_xfer_buf_alloc(blocks * block_len);
    if (!buffer) {
        vtl_scsi_staging_oom(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    if (vtl_scsi_copy_from_sg(cmd, buffer, blocks * block_len, &drv->sense)) {
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }
    ret = vtl_tape_write(drv, buffer, blocks * block_len, NULL);
    vtl_xfer_buf_free(buffer);

    if (ret < 0) {
        if (ret == -ENODEV)
            vtl_set_sense(&drv->sense, NOT_READY, 0x3a, 0);
        else if (ret == -EROFS)
            vtl_set_sense(&drv->sense, DATA_PROTECT, 0x27, 0);
        else if (ret == -ENOSPC)
            vtl_set_sense(&drv->sense, VOLUME_OVERFLOW, 0x00, 0);
        else
            vtl_set_sense(&drv->sense, MEDIUM_ERROR, 0x03, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    return SAM_STAT_GOOD;
}

static int vtl_handle_rewind(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    int ret = vtl_tape_rewind(drv);

    if (ret == -ENODEV) {
        vtl_set_sense(&drv->sense, NOT_READY, 0x3a, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    return SAM_STAT_GOOD;
}

static int vtl_handle_space(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    u8 *cdb = cmd->cmnd;
    int code, count;

    code = cdb[1] & 7;
    count = vtl_get_s24(&cdb[2]);

    if (vtl_tape_space(drv, code, count) == -ENODEV) {
        vtl_set_sense(&drv->sense, NOT_READY, 0x3a, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }
    return SAM_STAT_GOOD;
}

static int vtl_handle_write_filemarks(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    u8 *cdb = cmd->cmnd;
    int ret;
    u32 count;

    count = vtl_get_u24(&cdb[2]);
    ret = vtl_tape_write_filemarks(drv, count);
    if (ret < 0) {
        if (ret == -ENODEV)
            vtl_set_sense(&drv->sense, NOT_READY, 0x3a, 0);
        else if (ret == -EROFS)
            vtl_set_sense(&drv->sense, DATA_PROTECT, 0x27, 0);
        else
            vtl_set_sense(&drv->sense, MEDIUM_ERROR, 0x03, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }
    return SAM_STAT_GOOD;
}

static int vtl_handle_load_unload(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    u8 *cdb = cmd->cmnd;
    u8 load;

    load = cdb[4] & 0x01;

    if (load) {
        if (!vtl_drive_has_tape(drv)) {
            vtl_set_sense(&drv->sense, NOT_READY, 0x3a, 0);
            vtl_build_sense_buffer(cmd, &drv->sense);
            return SAM_STAT_CHECK_CONDITION;
        }
    } else {
        if (vtl_drive_has_tape(drv))
            vtl_tape_unload(drv);
    }

    return SAM_STAT_GOOD;
}

static int vtl_handle_move_medium(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    u8 *cdb = cmd->cmnd;
    struct vtl_changer *ch = vhost->changer;
    int src, dst;
    int ret;

    src = (cdb[2] << 8) | cdb[3];
    dst = (cdb[4] << 8) | cdb[5];

    ret = vtl_changer_move_medium(ch, src, dst);
    if (ret < 0) {
        vtl_set_sense(&ch->sense, ILLEGAL_REQUEST, 0x21, 0);
        vtl_build_sense_buffer(cmd, &ch->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    return SAM_STAT_GOOD;
}

/*
 * READ ELEMENT STATUS allocation / range: mtx and Linux ch(4) often use the
 * 6-byte form (alloc in cdb[5]); 10-byte uses cdb[7:8]; 12-byte (mtx) uses
 * cdb[7:9] as 24-bit length. Do not read cdb[9:11] on 12-byte CDBs — byte 9
 * is the LSB of alloc (e.g. 0xff → 255), not the MSB of a triplet at [9:11].
 */
static unsigned int vtl_res_alloc_len(const struct scsi_cmnd *cmd)
{
    const u8 *cdb = cmd->cmnd;

    if (cmd->cmd_len >= 12)
        return ((unsigned int)cdb[7] << 16) | ((unsigned int)cdb[8] << 8) |
	       cdb[9];
    if (cmd->cmd_len >= 10)
        return ((unsigned int)cdb[7] << 8) | cdb[8];
    if (cmd->cmd_len >= 6)
        return (unsigned int)cdb[5];
    return 0;
}

static void vtl_res_element_range(const struct scsi_cmnd *cmd, int *start, int *num)
{
    const u8 *cdb = cmd->cmnd;

    *start = (cdb[2] << 8) | cdb[3];
    if (cmd->cmd_len >= 10)
        *num = (cdb[4] << 8) | cdb[5];
    else if (cmd->cmd_len >= 6)
        *num = cdb[4];
    else
        *num = 0;
}

static int vtl_handle_read_element_status(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    u8 *cdb = cmd->cmnd;
    struct vtl_changer *ch = vhost->changer;
    u8 *buffer;
    unsigned int req_len;
    unsigned int work_len;
    int start_elem, num_elems;
    int ret;

    buffer = vtl_xfer_buf_alloc(VTL_ELEMENT_STATUS_BUFLEN);
    if (!buffer) {
        vtl_scsi_staging_oom(cmd, &ch->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    bool voltag;
    req_len = vtl_res_alloc_len(cmd);
    work_len = req_len ? min_t(unsigned int, req_len, VTL_ELEMENT_STATUS_BUFLEN)
			: VTL_ELEMENT_STATUS_BUFLEN;
    voltag = (cdb[1] & 0x10) != 0;
    /*
     * Backup apps / mistaken 12-byte CDB with cmd_len=10 parse alloc from bytes 7–8
     * only (e.g. …00 10 → 16 bytes) and see an “empty” library. mtx uses 10-byte
     * READ ELEMENT STATUS with a large alloc (255–0xffff). Floor at 4 KiB when
     * voltag is requested so inventory/mtx/sg agree.
     */
    if (!voltag && req_len >= 32U)
        voltag = true; /* Mars/Veritas often omit PV bit but expect barcodes */
    if (voltag && work_len < 4096U)
        work_len = min_t(unsigned int, VTL_ELEMENT_STATUS_BUFLEN, 4096U);
    if (cmd->cmd_len >= 10 && work_len < 4096U)
        work_len = min_t(unsigned int, VTL_ELEMENT_STATUS_BUFLEN, 4096U);
    if (work_len < 8)
        work_len = 8;
    vtl_res_element_range(cmd, &start_elem, &num_elems);
    ret = vtl_changer_read_element_status(
        ch, buffer, work_len, voltag, cdb[1] & 0x0f,
        start_elem, num_elems);
    if (ret < 0) {
        vtl_set_sense(&ch->sense, HARDWARE_ERROR, 0x00, 0x00);
        vtl_build_sense_buffer(cmd, &ch->sense);
        vtl_xfer_buf_free(buffer);
        return SAM_STAT_CHECK_CONDITION;
    }

    /*
     * Inline copy + one scsi_set_resid: avoids stacking residual from
     * vtl_scsi_copy_to_sg (short SG) with (req_len - ret) from allocation trim.
     */
    {
        int copied;

        copied = scsi_sg_copy_from_buffer(cmd, buffer, ret);
        if (unlikely(copied < 0)) {
            vtl_set_sense(&ch->sense, ABORTED_COMMAND, 0x00, 0x00);
            vtl_build_sense_buffer(cmd, &ch->sense);
            vtl_xfer_buf_free(buffer);
            return SAM_STAT_CHECK_CONDITION;
        }
        if (unlikely((unsigned int)copied < (unsigned int)ret)) {
            vtl_set_sense(&ch->sense, ABORTED_COMMAND, 0x00, 0x00);
            vtl_build_sense_buffer(cmd, &ch->sense);
            vtl_xfer_buf_free(buffer);
            return SAM_STAT_CHECK_CONDITION;
        }
        if (req_len > (unsigned int)copied)
            scsi_set_resid(cmd, req_len - (unsigned int)copied);
    }
    vtl_xfer_buf_free(buffer);

    return SAM_STAT_GOOD;
}

/*
 * LOG SENSE allocation length: 6-byte CDB uses byte 4; 10-byte uses bytes 7–8 (SPC).
 */
static unsigned int vtl_log_sense_alloc_len(struct scsi_cmnd *cmd)
{
    const u8 *cdb = cmd->cmnd;

    if (cmd->cmd_len >= 10)
        return (cdb[7] << 8) | cdb[8];
    if (cmd->cmd_len >= 6)
        return cdb[4];
    return 0;
}

/*
 * Minimal LOG SENSE so initiators probing pages get structured data.
 * Page 0x00: supported pages; 0x11: synthetic volume usage from vtl_tape_metadata.
 */
static int vtl_handle_log_sense(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    u8 *cdb = cmd->cmnd;
    u8 page = cdb[2] & 0x3f;
    unsigned int alloc = vtl_log_sense_alloc_len(cmd);
    u8 *buf;
    u16 out_len;
    unsigned int z;
    u64 log_bytes_read = 0;
    u64 log_bytes_written = 0;

    if (alloc == 0)
        return SAM_STAT_GOOD;

    z = min_t(unsigned int, alloc, 512U);
    buf = vtl_xfer_buf_alloc(z);
    if (!buf) {
        vtl_scsi_staging_oom(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    memset(buf, 0, z);
    out_len = 0;

    if (page == 0x00) {
        buf[0] = 0x00;
        buf[1] = 0;
        buf[2] = 0;
        buf[3] = 2;
        buf[4] = 0x00;
        buf[5] = 0x11;
        out_len = 6;
    } else if (page == 0x11) {
        mutex_lock(&drv->lock);
        if (!drv->loaded_tape) {
            mutex_unlock(&drv->lock);
            vtl_xfer_buf_free(buf);
            vtl_set_sense(&drv->sense, NOT_READY, 0x3a, 0);
            vtl_build_sense_buffer(cmd, &drv->sense);
            return SAM_STAT_CHECK_CONDITION;
        }
        mutex_lock(&drv->loaded_tape->lock);
        log_bytes_read = drv->loaded_tape->meta.log_bytes_read;
        log_bytes_written = drv->loaded_tape->meta.log_bytes_written;
        mutex_unlock(&drv->loaded_tape->lock);
        mutex_unlock(&drv->lock);
        buf[0] = 0x11;
        buf[1] = 0;
        buf[2] = 0;
        buf[3] = 16;
        vtl_put_be64(log_bytes_read, &buf[4]);
        vtl_put_be64(log_bytes_written, &buf[12]);
        out_len = 20;
    } else {
        vtl_xfer_buf_free(buf);
        vtl_set_sense(&drv->sense, ILLEGAL_REQUEST, 0x24, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    if (vtl_scsi_copy_to_sg(cmd, buf, min_t(unsigned int, out_len, z), &drv->sense)) {
        vtl_xfer_buf_free(buf);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buf);
    return SAM_STAT_GOOD;
}

/* SSC READ POSITION — long form service action 0x00 or 0x01 */
static int vtl_handle_read_position(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    u8 *cdb = cmd->cmnd;
    u8 *buf;
    u16 alloc;
    u8 svc = (cdb[1] & 0x1f);
    bool loaded;
    bool at_bot = false;
    bool at_end = false;
    bool at_filemark = false;
    loff_t position = 0;

    if (svc != 0x00 && svc != 0x01) {
        vtl_set_sense(&drv->sense, ILLEGAL_REQUEST, 0x24, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    if (cmd->cmd_len >= 10)
        alloc = (cdb[7] << 8) | cdb[8];
    else
        alloc = cdb[4];

    if (alloc < 20) {
        vtl_set_sense(&drv->sense, ILLEGAL_REQUEST, 0x24, 0);
        vtl_build_sense_buffer(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    buf = vtl_xfer_buf_alloc(20);
    if (!buf) {
        vtl_scsi_staging_oom(cmd, &drv->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    memset(buf, 0, 20);
    buf[0] = 0x80;
    mutex_lock(&drv->lock);
    loaded = drv->loaded_tape != NULL;
    if (loaded) {
        mutex_lock(&drv->loaded_tape->lock);
        at_bot = drv->at_bot;
        at_end = drv->at_end;
        at_filemark = drv->at_filemark;
        position = drv->loaded_tape->position;
        mutex_unlock(&drv->loaded_tape->lock);
    }
    mutex_unlock(&drv->lock);

    if (loaded) {
        if (at_bot)
            buf[1] |= 0x80;
        if (at_end)
            buf[1] |= 0x40;
        if (at_filemark)
            buf[1] |= 0x20;
        vtl_put_be64((u64)position, &buf[4]);
    } else {
        buf[1] |= 0x10;
    }

    if (vtl_scsi_copy_to_sg(cmd, buf, 20, &drv->sense)) {
        vtl_xfer_buf_free(buf);
        return SAM_STAT_CHECK_CONDITION;
    }
    vtl_xfer_buf_free(buf);
    return SAM_STAT_GOOD;
}

static int vtl_handle_prevent_allow(struct scsi_cmnd *cmd, struct vtl_drive *drv)
{
    (void)cmd;
    (void)drv;
    return SAM_STAT_GOOD;
}

/*
 * REPORT LUNS (SPC): lets scsi mid-layer enumerate 0..num_drives without
 * relying on sequential scan edge cases.
 */
static int vtl_handle_report_luns(struct scsi_cmnd *cmd, struct vtl_host *vhost)
{
    struct vtl_changer *ch = vhost->changer;
    u8 *cdb = cmd->cmnd;
    unsigned int nluns = (unsigned int)ch->num_drives + 1U;
    u32 list_len = nluns * (u32)sizeof(struct scsi_lun);
    u32 need = 8U + list_len;
    u32 alloc_len;
    u8 *buf;
    unsigned int i;
    struct scsi_lun *vec;
    unsigned int xfer;
    int err;

    if (cmd->cmd_len < 10)
        return vtl_cmd_illegal(cmd, &ch->sense);

    alloc_len = vtl_get_be32(&cdb[6]);
    if (alloc_len == 0)
        return SAM_STAT_GOOD;

    if (need > VTL_XFER_BUF_MAX)
        return vtl_cmd_illegal(cmd, &ch->sense);

    buf = vtl_xfer_buf_alloc((unsigned int)need);
    if (!buf) {
        vtl_scsi_staging_oom(cmd, &ch->sense);
        return SAM_STAT_CHECK_CONDITION;
    }

    xfer = min_t(u32, alloc_len, need);
    memset(buf, 0, (size_t)need);

    if (xfer >= 8) {
        vtl_put_be32(list_len, buf);
        vec = (struct scsi_lun *)(buf + 8);
        for (i = 0; i < nluns; i++) {
            if (8U + (i + 1U) * sizeof(struct scsi_lun) > xfer)
                break;
            int_to_scsilun(i, &vec[i]);
        }
    }

    err = vtl_scsi_copy_to_sg(cmd, buf, (unsigned int)xfer, &ch->sense);
    vtl_xfer_buf_free(buf);
    return err ? SAM_STAT_CHECK_CONDITION : SAM_STAT_GOOD;
}

static int vtl_changer_scsi(struct scsi_cmnd *cmd, struct vtl_host *vhost, u8 *cdb)
{
    struct vtl_changer *ch = vhost->changer;

    switch (cdb[0]) {
    case INQUIRY:
        return vtl_handle_inquiry(cmd, vhost);
    case TEST_UNIT_READY:
        return vtl_handle_test_unit_ready(cmd, vhost);
    case REQUEST_SENSE:
        return vtl_handle_request_sense(cmd, vhost);
    case MODE_SENSE:
    case MODE_SENSE_10:
        return vtl_handle_mode_sense(cmd, vhost);
    case MODE_SELECT:
    case MODE_SELECT_10:
        return vtl_handle_mode_select(cmd, vhost);
    case INITIALIZE_ELEMENT_STATUS:
        return SAM_STAT_GOOD;
    case PREVENT_ALLOW_MEDIUM_REMOVAL:
        return SAM_STAT_GOOD;
    case MOVE_MEDIUM:
        return vtl_handle_move_medium(cmd, vhost);
#if READ_ELEMENT_STATUS != 0xb4
    case 0xb4: /* READ ELEMENT STATUS (10); some backup stacks use b4 not b8 */
#endif
    case READ_ELEMENT_STATUS:
        return vtl_handle_read_element_status(cmd, vhost);
    case REPORT_LUNS:
        return vtl_handle_report_luns(cmd, vhost);
    default:
        return vtl_cmd_illegal(cmd, &ch->sense);
    }
}

static int vtl_tape_scsi(struct scsi_cmnd *cmd, struct vtl_host *vhost,
                         unsigned int drive_idx, u8 *cdb)
{
    struct vtl_changer *ch = vhost->changer;
    struct vtl_drive *drv;

    if (drive_idx >= (unsigned int)ch->num_drives)
        return vtl_cmd_lun_not_supported(cmd, ch);

    drv = &ch->drives[drive_idx];

    switch (cdb[0]) {
    case INQUIRY:
        return vtl_handle_inquiry(cmd, vhost);
    case TEST_UNIT_READY:
        return vtl_handle_test_unit_ready(cmd, vhost);
    case REQUEST_SENSE:
        return vtl_handle_request_sense(cmd, vhost);
    case READ_BLOCK_LIMITS:
        return vtl_handle_read_block_limits(cmd, drv);
    case MODE_SENSE:
    case MODE_SENSE_10:
        return vtl_handle_mode_sense(cmd, vhost);
    case MODE_SELECT:
    case MODE_SELECT_10:
        return vtl_handle_mode_select(cmd, vhost);
    case READ_6:
    case READ_10:
    case READ_12:
        return vtl_handle_read(cmd, drv, cdb[0]);
    case WRITE_6:
    case WRITE_10:
    case WRITE_12:
        return vtl_handle_write(cmd, drv, cdb[0]);
    case REWIND:
        return vtl_handle_rewind(cmd, drv);
    case SPACE:
        return vtl_handle_space(cmd, drv);
    case WRITE_FILEMARKS:
        return vtl_handle_write_filemarks(cmd, drv);
    case LOAD_UNLOAD:
        return vtl_handle_load_unload(cmd, drv);
    case LOG_SENSE:
        return vtl_handle_log_sense(cmd, drv);
    case READ_POSITION:
        return vtl_handle_read_position(cmd, drv);
    case PREVENT_ALLOW_MEDIUM_REMOVAL:
        return vtl_handle_prevent_allow(cmd, drv);
    default:
        return vtl_cmd_illegal(cmd, &drv->sense);
    }
}

int vtl_scsi_queuecommand(struct Scsi_Host *shost, struct scsi_cmnd *cmd)
{
    struct vtl_host *vhost = shost_priv(shost);
    u8 *cdb = cmd->cmnd;
    unsigned int lun = cmd->device->lun;
    struct vtl_changer *ch;
    int result;

    if (vtl_reconfig_in_progress() || !vhost->changer) {
        cmd->result = (DID_NO_CONNECT << 16);
        vtl_scsi_done(cmd);
        return 0;
    }

    ch = vhost->changer;

    /* Only virtual target 0 / channel 0 is implemented */
    if (cmd->device->channel != 0 || cmd->device->id != 0) {
        cmd->result = (DID_BAD_TARGET << 16);
        vtl_scsi_done(cmd);
        return 0;
    }

    if (lun > (unsigned int)ch->num_drives) {
        result = vtl_cmd_lun_not_supported(cmd, ch);
        goto out;
    }

    if (lun == 0)
        result = vtl_changer_scsi(cmd, vhost, cdb);
    else
        result = vtl_tape_scsi(cmd, vhost, lun - 1, cdb);

out:
    vtl_set_cmd_result(cmd, result);
    vtl_scsi_done(cmd);

    return 0;
}

int vtl_slave_alloc(struct scsi_device *sdev)
{
    return 0;
}

void vtl_slave_destroy(struct scsi_device *sdev)
{
}

int vtl_slave_configure(struct scsi_device *sdev)
{
    return 0;
}

int vtl_change_queue_depth(struct scsi_device *sdev, int depth)
{
    return depth;
}
