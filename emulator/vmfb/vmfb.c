// vmfb.c - fake framebuffer for the Emaldo firmware emulator.
//
// Registers a real fbdev device (/dev/fb0) backed by vmalloc'd memory so the
// vendor app's mappedFramebuffer mmap works against real kernel pages. The
// pixel buffer is simultaneously exported through debugfs as a blob
// (/sys/kernel/debug/vmfb/framebuffer) plus a small control file, so a
// userspace renderer can read the exact bytes the app drew.
//
// Panel parameters are guesses at the real e-ink part (monochrome); the app
// reads them from FBIOGET_VSCREENINFO, so we define the geometry here.
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/fb.h>
#include <linux/vmalloc.h>
#include <linux/debugfs.h>
#include <linux/uaccess.h>
#include <linux/mutex.h>

#define VMFB_XRES  400
#define VMFB_YRES  300
#define VMFB_BPP   32          // report 32bpp RGBA-ish; simple to render

static struct fb_info *vmfb_info;
static struct dentry *dbg_dir;
static DEFINE_MUTEX(vmfb_lock);

static int vmfb_mmap(struct fb_info *info, struct vm_area_struct *vma)
{
    unsigned long off = vma->vm_pgoff << PAGE_SHIFT;
    unsigned long size = vma->vm_end - vma->vm_start;

    if (off + size > info->fix.smem_len)
        return -EINVAL;
    // mmap against the vmalloc area needs remap_vmalloc_range
    return remap_vmalloc_range(vma, info->screen_base, 0);
}

static int vmfb_blank(int blank, struct fb_info *info)
{
    return 0;  // pretend the panel always accepts blanking
}

static ssize_t vmfb_read(struct fb_info *info, char __user *buf,
                         size_t count, loff_t *ppos)
{
    unsigned long p = *ppos;
    if (p >= info->fix.smem_len)
        return 0;
    if (count > info->fix.smem_len - p)
        count = info->fix.smem_len - p;
    if (copy_to_user(buf, info->screen_base + p, count))
        return -EFAULT;
    *ppos += count;
    return count;
}

static ssize_t vmfb_write(struct fb_info *info, const char __user *buf,
                          size_t count, loff_t *ppos)
{
    unsigned long p = *ppos;
    if (p >= info->fix.smem_len)
        return -ENOSPC;
    if (count > info->fix.smem_len - p)
        count = info->fix.smem_len - p;
    if (copy_from_user(info->screen_base + p, buf, count))
        return -EFAULT;
    *ppos += count;
    return count;
}

static struct fb_ops vmfb_ops = {
    .owner          = THIS_MODULE,
    .fb_read        = vmfb_read,
    .fb_write       = vmfb_write,
    .fb_fillrect    = sys_fillrect,
    .fb_copyarea    = sys_copyarea,
    .fb_imageblit   = sys_imageblit,
    .fb_mmap        = vmfb_mmap,
    .fb_blank       = vmfb_blank,
};

/* debugfs: read the live framebuffer bytes */
static ssize_t vmfb_frame_read(struct file *f, char __user *buf,
                               size_t len, loff_t *ppos)
{
    ssize_t r;
    mutex_lock(&vmfb_lock);
    r = simple_read_from_buffer(buf, len, ppos,
                                vmfb_info->screen_base,
                                vmfb_info->fix.smem_len);
    mutex_unlock(&vmfb_lock);
    return r;
}
static const struct file_operations vmfb_frame_fops = {
    .owner = THIS_MODULE,
    .read  = vmfb_frame_read,
    .llseek = default_llseek,
};

static int __init vmfb_init(void)
{
    unsigned long smem = VMFB_XRES * VMFB_YRES * (VMFB_BPP / 8);

    vmfb_info = framebuffer_alloc(0, NULL);
    if (!vmfb_info)
        return -ENOMEM;

    vmfb_info->screen_base = vzalloc(smem);
    if (!vmfb_info->screen_base) {
        framebuffer_release(vmfb_info);
        return -ENOMEM;
    }

    strcpy(vmfb_info->fix.id, "vmfb");
    vmfb_info->fix.smem_start = 0;      // vmalloc: no physical address
    vmfb_info->fix.smem_len   = smem;
    vmfb_info->fix.type       = FB_TYPE_PACKED_PIXELS;
    vmfb_info->fix.visual     = FB_VISUAL_TRUECOLOR;
    vmfb_info->fix.line_length = VMFB_XRES * (VMFB_BPP / 8);
    vmfb_info->fix.accel      = FB_ACCEL_NONE;

    vmfb_info->var.xres           = VMFB_XRES;
    vmfb_info->var.yres           = VMFB_YRES;
    vmfb_info->var.xres_virtual   = VMFB_XRES;
    vmfb_info->var.yres_virtual   = VMFB_YRES;
    vmfb_info->var.bits_per_pixel = VMFB_BPP;
    vmfb_info->var.red.offset     = 16; vmfb_info->var.red.length   = 8;
    vmfb_info->var.green.offset   = 8;  vmfb_info->var.green.length = 8;
    vmfb_info->var.blue.offset    = 0;  vmfb_info->var.blue.length  = 8;
    vmfb_info->var.transp.offset  = 24; vmfb_info->var.transp.length = 8;
    vmfb_info->var.activate       = FB_ACTIVATE_NOW;
    vmfb_info->var.height         = 55;  // ~2.2" panel, informational
    vmfb_info->var.width          = 73;

    vmfb_info->fbops = &vmfb_ops;
    vmfb_info->flags = FBINFO_DEFAULT | FBINFO_VIRTFB;

    if (register_framebuffer(vmfb_info) < 0) {
        vfree(vmfb_info->screen_base);
        framebuffer_release(vmfb_info);
        return -EINVAL;
    }
    pr_info("vmfb: registered /dev/fb0 %dx%d@%d, %lu bytes\n",
            VMFB_XRES, VMFB_YRES, VMFB_BPP, smem);

    dbg_dir = debugfs_create_dir("vmfb", NULL);
    debugfs_create_file("framebuffer", 0444, dbg_dir, NULL,
                        &vmfb_frame_fops);
    debugfs_create_u32("xres", 0444, dbg_dir,
                       (u32 *)&vmfb_info->var.xres);
    debugfs_create_u32("yres", 0444, dbg_dir,
                       (u32 *)&vmfb_info->var.yres);
    return 0;
}

static void __exit vmfb_exit(void)
{
    debugfs_remove_recursive(dbg_dir);
    unregister_framebuffer(vmfb_info);
    vfree(vmfb_info->screen_base);
    framebuffer_release(vmfb_info);
}

module_init(vmfb_init);
module_exit(vmfb_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Emaldo emulator fake framebuffer");
