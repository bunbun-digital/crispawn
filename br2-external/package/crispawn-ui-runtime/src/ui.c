// SPDX-License-Identifier: Apache-2.0
#include "crispawn/ui.h"

#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <linux/kd.h>
#include <linux/vt.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define FONT_PATH "/usr/share/fonts/crispawn/noto-sans-jp.otf"
#define MAX_KEYBOARDS 32
#define BITS_PER_LONG (8U * sizeof(unsigned long))

struct font_size {
    uint32_t pixels;
    lv_font_t *font;
    struct font_size *next;
};

struct crispawn_ui {
    int lock_fd, tty_fd, old_mode;
    bool initialized, restore_console;
    bool vt_owned, vt_active;
    struct vt_mode old_vt_mode;
    struct sigaction old_release, old_acquire;
    uint64_t input_sequence;
    uint32_t keyboard_retry_ms;
    void (*key_callback)(void *, uint32_t);
    void *key_callback_data;
    lv_display_t *display;
    lv_group_t *group;
    lv_font_t *font;
    struct font_size *fonts;
    lv_indev_t *keyboards[MAX_KEYBOARDS];
    unsigned keyboard_count;
};

static crispawn_ui *active;
static volatile sig_atomic_t vt_release_requested, vt_acquire_requested;
static void vt_release(int sig) { (void)sig; vt_release_requested = 1; }
static void vt_acquire(int sig) { (void)sig; vt_acquire_requested = 1; }

static uint32_t monotonic_ms(void)
{
    struct timespec ts = {0};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint32_t)((uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000);
}

static lv_key_t navigation(lv_indev_t *indev, lv_key_t key)
{
    (void)indev;
    /* LVGL's evdev adapter represents non-navigation Linux keys as code+0xffff.
     * Use physical letter positions; Shift/Caps Lock need no text input stack. */
    if (key >= 0xffff) {
        static const char letters[] = {
            [KEY_A] = 'a', [KEY_B] = 'b', [KEY_C] = 'c', [KEY_D] = 'd',
            [KEY_E] = 'e', [KEY_F] = 'f', [KEY_G] = 'g', [KEY_H] = 'h',
            [KEY_I] = 'i', [KEY_J] = 'j', [KEY_K] = 'k', [KEY_L] = 'l',
            [KEY_M] = 'm', [KEY_N] = 'n', [KEY_O] = 'o', [KEY_P] = 'p',
            [KEY_Q] = 'q', [KEY_R] = 'r', [KEY_S] = 's', [KEY_T] = 't',
            [KEY_U] = 'u', [KEY_V] = 'v', [KEY_W] = 'w', [KEY_X] = 'x',
            [KEY_Y] = 'y', [KEY_Z] = 'z', [KEY_0] = '0', [KEY_1] = '1',
            [KEY_2] = '2', [KEY_3] = '3', [KEY_4] = '4', [KEY_5] = '5',
            [KEY_6] = '6', [KEY_7] = '7', [KEY_8] = '8', [KEY_9] = '9',
        };
        unsigned code = key - 0xffff;
        if (code == KEY_PAGEUP) return CRISPAWN_KEY_PAGE_UP;
        if (code == KEY_PAGEDOWN) return CRISPAWN_KEY_PAGE_DOWN;
        if (code == KEY_END) return CRISPAWN_KEY_END;
        return code < sizeof(letters) ? (uint32_t)letters[code] : 0;
    }
    if (key == LV_KEY_UP || key == LV_KEY_LEFT) return LV_KEY_PREV;
    if (key == LV_KEY_DOWN || key == LV_KEY_RIGHT) return LV_KEY_NEXT;
    if (key == LV_KEY_END) return CRISPAWN_KEY_END;
    return key;
}

static bool has_key(const unsigned long *bits, unsigned key)
{
    return (bits[key / BITS_PER_LONG] & (1UL << (key % BITS_PER_LONG))) != 0;
}
static void keyboard_event(lv_event_t *event)
{
    lv_indev_t *indev = lv_event_get_target(event);
    if (active && lv_indev_get_state(indev) == LV_INDEV_STATE_PRESSED) {
        ++active->input_sequence;
        if (active->key_callback)
            active->key_callback(active->key_callback_data, lv_indev_get_key(indev));
    }
}

static void open_keyboards(crispawn_ui *ui)
{
    for (unsigned i = 0; i < MAX_KEYBOARDS; ++i) {
        char path[64];
        unsigned long keys[(KEY_MAX + BITS_PER_LONG) / BITS_PER_LONG] = {0};
        snprintf(path, sizeof(path), "/dev/input/event%u", i);
        int fd = open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
        if (fd < 0) continue;
        bool keyboard = ioctl(fd, EVIOCGBIT(EV_KEY, sizeof(keys)), keys) >= 0 &&
                        has_key(keys, KEY_ENTER) && has_key(keys, KEY_TAB);
        close(fd);
        if (!keyboard) continue;
        lv_indev_t *indev = lv_evdev_create(LV_INDEV_TYPE_KEYPAD, path);
        if (!indev) continue;
        lv_indev_set_display(indev, ui->display);
        lv_indev_set_group(indev, ui->key_callback ? NULL : ui->group);
        lv_indev_set_key_remap_cb(indev, navigation);
        lv_indev_add_event_cb(indev, keyboard_event, LV_EVENT_KEY, NULL);
        ui->keyboards[ui->keyboard_count++] = indev;
    }
}

static bool verify_japanese_font(const lv_font_t *font)
{
    /* 日本語, 設定, 接続, バックアップ, 資料: check the actual loaded font. */
    const uint32_t letters[] = {
        0x65e5, 0x672c, 0x8a9e, 0x8a2d, 0x5b9a, 0x63a5, 0x7d9a,
        0x30d0, 0x30c3, 0x30af, 0x30a2, 0x30d7, 0x8cc7, 0x6599
    };
    for (size_t i = 0; i < sizeof(letters) / sizeof(letters[0]); ++i) {
        lv_font_glyph_dsc_t glyph = {0};
        if (!lv_font_get_glyph_dsc(font, &glyph, letters[i], 0) || glyph.is_placeholder) {
            fprintf(stderr, "CRISPAWN_UI_FAIL missing glyph U+%04X\n", letters[i]);
            return false;
        }
        lv_font_glyph_release_draw_data(&glyph);
    }
    printf("CRISPAWN_UI_FONT_OK lang=ja font=NotoSansJP glyphs=%zu path=%s\n",
           sizeof(letters) / sizeof(letters[0]), FONT_PATH);
    return true;
}

static crispawn_ui *open_ui(bool require_keyboard)
{
    if (active) {
        errno = EBUSY;
        return NULL;
    }
    crispawn_ui *ui = calloc(1, sizeof(*ui));
    if (!ui) return NULL;
    ui->lock_fd = ui->tty_fd = -1;
    ui->lock_fd = open("/run/crispawn-ui.lock", O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (ui->lock_fd < 0 || flock(ui->lock_fd, LOCK_EX | LOCK_NB)) {
        perror("CRISPAWN_UI_FAIL display ownership");
        goto fail;
    }
    for (unsigned i = 0; i < 100 && access("/dev/fb0", R_OK | W_OK); ++i)
        usleep(100000);
    if (access("/dev/fb0", R_OK | W_OK)) {
        perror("CRISPAWN_UI_FAIL framebuffer");
        goto fail;
    }

    lv_init();
    ui->initialized = true;
    lv_tick_set_cb(monotonic_ms);
    ui->display = lv_linux_fbdev_create();
    if (!ui->display || lv_linux_fbdev_set_file(ui->display, "/dev/fb0") != LV_RESULT_OK) {
        fprintf(stderr, "CRISPAWN_UI_FAIL LVGL framebuffer initialization\n");
        goto fail;
    }
    ui->font = lv_freetype_font_create(FONT_PATH, LV_FREETYPE_FONT_RENDER_MODE_BITMAP,
                                     24, LV_FREETYPE_FONT_STYLE_NORMAL);
    if (!ui->font || !verify_japanese_font(ui->font)) {
        fprintf(stderr, "CRISPAWN_UI_FAIL Japanese font initialization\n");
        goto fail;
    }
    ui->group = lv_group_create();
    if (!ui->group) goto fail;
    lv_group_set_wrap(ui->group, true);
    lv_theme_t *theme = lv_theme_default_init(ui->display, lv_color_hex(0x247e72),
                                             lv_color_hex(0x25455b), true, ui->font);
    if (!theme) goto fail;
    lv_display_set_theme(ui->display, theme);
    lv_obj_set_style_text_font(lv_display_get_screen_active(ui->display), ui->font, 0);
    open_keyboards(ui);
    if (require_keyboard && !ui->keyboard_count) {
        fprintf(stderr, "CRISPAWN_UI_FAIL no evdev keyboard\n");
        goto fail;
    }

    /* Own tty1 explicitly. tty0 refers to whichever VT is active at the time. */
    ui->tty_fd = open("/dev/tty1", O_RDWR | O_CLOEXEC);
    if (ui->tty_fd >= 0) {
        struct sigaction release = {0}, acquire = {0};
        release.sa_handler = vt_release; acquire.sa_handler = vt_acquire;
        sigemptyset(&release.sa_mask); sigemptyset(&acquire.sa_mask);
        struct vt_mode mode = {0};
        mode.mode = VT_PROCESS; mode.relsig = SIGUSR1; mode.acqsig = SIGUSR2;
        vt_release_requested = vt_acquire_requested = 0;
        if (ioctl(ui->tty_fd, VT_GETMODE, &ui->old_vt_mode) != 0) goto fail;
        if (sigaction(SIGUSR1, &release, &ui->old_release) != 0) goto fail;
        if (sigaction(SIGUSR2, &acquire, &ui->old_acquire) != 0) {
            sigaction(SIGUSR1, &ui->old_release, NULL); goto fail;
        }
        if (ioctl(ui->tty_fd, VT_SETMODE, &mode) != 0) {
            sigaction(SIGUSR1, &ui->old_release, NULL);
            sigaction(SIGUSR2, &ui->old_acquire, NULL); goto fail;
        }
        ui->vt_owned = true;
        if (ioctl(ui->tty_fd, VT_ACTIVATE, 1) != 0) goto fail;
        int rc;
        do { rc = ioctl(ui->tty_fd, VT_WAITACTIVE, 1); } while (rc != 0 && errno == EINTR);
        if (rc != 0) goto fail;
        ui->vt_active = true;
    }
    if (ui->tty_fd < 0) goto fail;
    if (ui->tty_fd >= 0 && ioctl(ui->tty_fd, KDGETMODE, &ui->old_mode) == 0 &&
        ioctl(ui->tty_fd, KDSETMODE, KD_GRAPHICS) == 0)
        ui->restore_console = true;
    active = ui;
    return ui;
fail:
    crispawn_ui_close(ui);
    return NULL;
}

crispawn_ui *crispawn_ui_open(void) { return open_ui(true); }
crispawn_ui *crispawn_ui_open_display_only(void) { return open_ui(false); }
lv_display_t *crispawn_ui_display(crispawn_ui *ui) { return ui ? ui->display : NULL; }
lv_group_t *crispawn_ui_group(crispawn_ui *ui) { return ui ? ui->group : NULL; }
const lv_font_t *crispawn_ui_font(crispawn_ui *ui) { return ui ? ui->font : NULL; }
const lv_font_t *crispawn_ui_font_size(crispawn_ui *ui, uint32_t pixels)
{
    if (!ui || !pixels) { errno = EINVAL; return NULL; }
    if (pixels == 24) return ui->font;
    for (struct font_size *entry = ui->fonts; entry; entry = entry->next)
        if (entry->pixels == pixels) return entry->font;
    struct font_size *entry = calloc(1, sizeof(*entry));
    if (!entry) return NULL;
    entry->font = lv_freetype_font_create(FONT_PATH, LV_FREETYPE_FONT_RENDER_MODE_BITMAP,
                                         pixels, LV_FREETYPE_FONT_STYLE_NORMAL);
    if (!entry->font) { free(entry); return NULL; }
    entry->pixels = pixels;
    entry->next = ui->fonts;
    ui->fonts = entry;
    return entry->font;
}
unsigned crispawn_ui_keyboard_count(crispawn_ui *ui) { return ui ? ui->keyboard_count : 0; }
uint64_t crispawn_ui_input_sequence(crispawn_ui *ui) { return ui ? ui->input_sequence : 0; }
void crispawn_ui_set_key_callback(crispawn_ui *ui, void (*callback)(void *, uint32_t), void *data)
{
    if (!ui) return;
    ui->key_callback = callback;
    ui->key_callback_data = data;
    for (unsigned i = 0; i < ui->keyboard_count; ++i)
        lv_indev_set_group(ui->keyboards[i], callback ? NULL : ui->group);
}

uint32_t crispawn_ui_step(crispawn_ui *ui)
{
    if (!ui || ui != active) return 20;
    if (ui->vt_owned && vt_release_requested) {
        vt_release_requested = 0;
        for (unsigned i = 0; i < ui->keyboard_count; ++i) lv_evdev_delete(ui->keyboards[i]);
        ui->keyboard_count = 0;
        ui->vt_active = false;
        ioctl(ui->tty_fd, VT_RELDISP, 1);
        puts("CRISPAWN_UI_VT_RELEASED tty=1 painting=false");
        fflush(stdout);
    }
    if (ui->vt_owned && vt_acquire_requested) {
        vt_acquire_requested = 0;
        ioctl(ui->tty_fd, VT_RELDISP, VT_ACKACQ);
        ui->vt_active = true;
        if (!ui->keyboard_count) open_keyboards(ui); /* Discard keys typed on the diagnostic VT. */
        lv_obj_invalidate(lv_display_get_screen_active(ui->display));
        puts("CRISPAWN_UI_VT_ACQUIRED tty=1 repaint=true");
        fflush(stdout);
    }
    if (ui->vt_owned && !ui->vt_active) return 20;
    /* Early boot can precede USB keyboard enumeration. Never require input for
     * splash drawing, but attach a keyboard as soon as one is available. */
    uint32_t now = monotonic_ms();
    if (!ui->keyboard_count && now - ui->keyboard_retry_ms >= 1000) {
        ui->keyboard_retry_ms = now;
        open_keyboards(ui);
    }
    uint32_t delay = lv_timer_handler();
    if (delay > 20) delay = 20;
    return delay ? delay : 1;
}

void crispawn_ui_close(crispawn_ui *ui)
{
    if (!ui) return;
    for (unsigned i = 0; i < ui->keyboard_count; ++i) lv_evdev_delete(ui->keyboards[i]);
    if (ui->display) lv_display_delete(ui->display);
    if (ui->group) lv_group_delete(ui->group);
    while (ui->fonts) {
        struct font_size *entry = ui->fonts;
        ui->fonts = entry->next;
        lv_freetype_font_delete(entry->font);
        free(entry);
    }
    if (ui->font) lv_freetype_font_delete(ui->font);
    if (ui->initialized) lv_deinit();
    if (ui->restore_console) ioctl(ui->tty_fd, KDSETMODE, ui->old_mode);
    if (ui->vt_owned) {
        ioctl(ui->tty_fd, VT_SETMODE, &ui->old_vt_mode);
        sigaction(SIGUSR1, &ui->old_release, NULL);
        sigaction(SIGUSR2, &ui->old_acquire, NULL);
    }
    if (ui->tty_fd >= 0) close(ui->tty_fd);
    if (ui->lock_fd >= 0) close(ui->lock_fd);
    if (active == ui) active = NULL;
    free(ui);
}
