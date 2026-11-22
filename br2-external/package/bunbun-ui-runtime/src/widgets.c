// SPDX-License-Identifier: Apache-2.0
/* SPDX-License-Identifier: LicenseRef-Bunbun-Proprietary */
/* Generic LVGL bridge. No product strings, layouts or progress calculations. */
#include "bunbun/ui.h"
#include "bunbun/widgets.h"
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <signal.h>

struct click_binding { struct bunbun_scene *scene; uint32_t action; lv_obj_t *widget; };
struct key_binding { lv_obj_t *widget; uint32_t key; };
struct image_binding { lv_obj_t *widget; lv_image_dsc_t descriptor; uint8_t *pixels; };
struct bunbun_scene {
    bunbun_ui *runtime;
    lv_display_t *display;
    lv_group_t *group;
    lv_indev_t *input;
    char *font_path;
    lv_font_t *fonts[257];
    uint8_t *pixels;
    uint32_t width, height, key;
    uint64_t input_sequence;
    lv_indev_state_t key_state;
    struct click_binding clicks[128];
    struct key_binding shortcuts[128];
    struct image_binding images[16];
    uint32_t image_count;
    uint32_t shortcut_count;
    uint32_t click_count, actions[64], read, write, count;
    uint32_t input_keys[64], input_read, input_write, input_count;
    bool memory, initialized, overflow, direct_keys, command_input;
    void (*old_int)(int), (*old_term)(int);
    bool signals;
};
static bunbun_scene *active_scene;
static volatile sig_atomic_t quit_requested;
static void stop(int sig) { (void)sig; quit_requested = 1; }
static uint32_t milliseconds(void) {
    struct timespec t = {0}; clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint32_t)((uint64_t)t.tv_sec * 1000 + (uint64_t)t.tv_nsec / 1000000);
}
static void flush_memory(lv_display_t *display, const lv_area_t *area, uint8_t *pixels) {
    (void)area; (void)pixels; lv_display_flush_ready(display);
}
static void read_memory(lv_indev_t *input, lv_indev_data_t *data) {
    bunbun_scene *s = lv_indev_get_user_data(input);
    data->key = s->key; data->state = s->key_state;
}
static uint32_t command_key(uint32_t key) {
    if (key >= 'A' && key <= 'Z') return key + ('a' - 'A');
    return ((key >= 'a' && key <= 'z') || (key >= '0' && key <= '9')) ? key : 0;
}
static void dispatch_key(void *data, uint32_t key) {
    bunbun_scene *s = data;
    if (s->command_input) {
        if (key >= 'A' && key <= 'Z') key += 'a' - 'A';
        if (!(command_key(key) || key == LV_KEY_ESC || key == BUNBUN_KEY_PAGE_UP ||
              key == BUNBUN_KEY_PAGE_DOWN || key == BUNBUN_KEY_END)) return;
        if (s->input_count == 64) { s->overflow = true; return; }
        s->input_keys[s->input_write] = key;
        s->input_write = (s->input_write + 1) % 64;
        ++s->input_count;
        return;
    }
    if (!s->direct_keys || !(key = command_key(key))) return;
    for (unsigned i = 0; i < s->shortcut_count; ++i) {
        struct key_binding *b = &s->shortcuts[i];
        if (b->key != key) continue;
        for (lv_obj_t *o = b->widget; o; o = lv_obj_get_parent(o))
            if (lv_obj_has_state(o, LV_STATE_DISABLED) || lv_obj_is_hidden(o)) return;
        lv_obj_send_event(b->widget, LV_EVENT_CLICKED, NULL);
        return;
    }
}
static void memory_key(lv_event_t *event) {
    bunbun_scene *s = lv_event_get_user_data(event);
    lv_indev_t *indev = lv_event_get_target(event);
    if (lv_indev_get_state(indev) == LV_INDEV_STATE_PRESSED) {
        ++s->input_sequence;
        dispatch_key(s, lv_indev_get_key(indev));
    }
}
uint32_t bunbun_widgets_api_version(void) { return 1; }
static bunbun_scene *allocate_scene(void) {
    if (active_scene) return NULL;
    return calloc(1, sizeof(bunbun_scene));
}
static bunbun_scene *open_display(bool require_keyboard) {
    bunbun_scene *s = allocate_scene(); if (!s) return NULL;
    s->runtime = require_keyboard ? bunbun_ui_open() : bunbun_ui_open_display_only();
    if (!s->runtime) { free(s); return NULL; }
    s->display = bunbun_ui_display(s->runtime); s->group = bunbun_ui_group(s->runtime);
    quit_requested = 0;
    s->old_int = signal(SIGINT, stop); s->old_term = signal(SIGTERM, stop);
    s->signals = true;
    active_scene = s; return s;
}
bunbun_scene *bunbun_scene_open(void) { return open_display(true); }
bunbun_scene *bunbun_scene_open_display_only(void) { return open_display(false); }
bunbun_scene *bunbun_scene_open_memory(uint32_t w, uint32_t h, const char *font) {
    if (!font || !w || !h || w > 4096 || h > 4096) return NULL;
    bunbun_scene *s = allocate_scene(); if (!s) return NULL;
    s->memory = true; s->width = w; s->height = h;
    s->font_path = strdup(font); s->pixels = calloc((size_t)w * h, 4);
    if (!s->font_path || !s->pixels) goto fail;
    lv_init(); s->initialized = true; lv_tick_set_cb(milliseconds);
    s->display = lv_display_create((int32_t)w, (int32_t)h);
    s->group = lv_group_create();
    if (!s->display || !s->group) goto fail;
    lv_display_set_color_format(s->display, LV_COLOR_FORMAT_XRGB8888);
    lv_display_set_flush_cb(s->display, flush_memory);
    lv_display_set_buffers(s->display, s->pixels, NULL, w * h * 4, LV_DISPLAY_RENDER_MODE_FULL);
    lv_group_set_wrap(s->group, true);
    s->input = lv_indev_create(); if (!s->input) goto fail;
    lv_indev_set_type(s->input, LV_INDEV_TYPE_KEYPAD);
    lv_indev_set_read_cb(s->input, read_memory); lv_indev_set_user_data(s->input, s);
    lv_indev_set_display(s->input, s->display); lv_indev_set_group(s->input, s->group);
    lv_indev_add_event_cb(s->input, memory_key, LV_EVENT_KEY, s);
    active_scene = s; return s;
fail:
    bunbun_scene_close(s); return NULL;
}
void bunbun_scene_close(bunbun_scene *s) {
    if (!s) return;
    if (s->runtime) bunbun_ui_close(s->runtime);
    else {
        if (s->input) lv_indev_delete(s->input);
        if (s->display) lv_display_delete(s->display);
        if (s->group) lv_group_delete(s->group);
        for (unsigned i = 0; i < 257; ++i) if (s->fonts[i]) lv_freetype_font_delete(s->fonts[i]);
        if (s->initialized) lv_deinit();
    }
    for (unsigned i = 0; i < s->image_count; ++i) free(s->images[i].pixels);
    free(s->pixels); free(s->font_path);
    if (s->signals) {
        if (s->old_int != SIG_ERR) signal(SIGINT, s->old_int);
        if (s->old_term != SIG_ERR) signal(SIGTERM, s->old_term);
    }
    if (active_scene == s) active_scene = NULL;
    free(s);
}
uint32_t bunbun_scene_step(bunbun_scene *s) {
    if (s->runtime) return bunbun_ui_step(s->runtime);
    uint32_t delay = lv_timer_handler(); return delay > 20 ? 20 : (delay ? delay : 1);
}
uint32_t bunbun_scene_keyboards(bunbun_scene *s) {
    return s->runtime ? bunbun_ui_keyboard_count(s->runtime) : 0;
}
uint64_t bunbun_scene_input_sequence(bunbun_scene *s) {
    return s->runtime ? bunbun_ui_input_sequence(s->runtime) : s->input_sequence;
}
uint32_t bunbun_scene_should_quit(bunbun_scene *s) { return !s->memory && quit_requested; }
int bunbun_scene_action(bunbun_scene *s, uint32_t *action) {
    if (s->overflow) return -1;
    if (!s->count) return 0;
    *action = s->actions[s->read]; s->read = (s->read + 1) % 64; --s->count; return 1;
}
bunbun_widget *bunbun_scene_root(bunbun_scene *s) {
    return (bunbun_widget *)lv_display_get_screen_active(s->display);
}
bunbun_font *bunbun_scene_font(bunbun_scene *s, uint32_t pixels) {
    if (!pixels || pixels > 256) return NULL;
    if (s->runtime) return (bunbun_font *)bunbun_ui_font_size(s->runtime, pixels);
    if (!s->fonts[pixels]) s->fonts[pixels] = lv_freetype_font_create(s->font_path,
        LV_FREETYPE_FONT_RENDER_MODE_BITMAP, pixels, LV_FREETYPE_FONT_STYLE_NORMAL);
    return (bunbun_font *)s->fonts[pixels];
}
int bunbun_scene_theme(bunbun_scene *s, uint32_t primary, uint32_t secondary, const bunbun_font *font) {
    lv_theme_t *theme = lv_theme_default_init(s->display, lv_color_hex(primary), lv_color_hex(secondary),
                                             false, (const lv_font_t *)font);
    if (!theme) return -1;
    lv_display_set_theme(s->display, theme); return 0;
}
int bunbun_scene_render_rgb(bunbun_scene *s, uint8_t *rgb, size_t length) {
    if (!s->memory || !rgb || length != (size_t)s->width * s->height * 3) return -1;
    lv_refr_now(s->display);
    for (size_t i = 0; i < (size_t)s->width * s->height; ++i) {
        rgb[3*i] = s->pixels[4*i+2]; rgb[3*i+1] = s->pixels[4*i+1]; rgb[3*i+2] = s->pixels[4*i];
    }
    return 0;
}
int bunbun_scene_send_key(bunbun_scene *s, uint32_t key) {
    bool special = s->command_input && (key == LV_KEY_ESC || key == BUNBUN_KEY_PAGE_UP ||
                                       key == BUNBUN_KEY_PAGE_DOWN || key == BUNBUN_KEY_END);
    if (!s->memory || !((key >= 1 && key <= 3) || command_key(key) || special)) return -1;
    s->key = key == 1 ? LV_KEY_NEXT : key == 2 ? LV_KEY_PREV : key == 3 ? LV_KEY_ENTER : key;
    s->key_state = LV_INDEV_STATE_PRESSED; lv_indev_read(s->input);
    s->key_state = LV_INDEV_STATE_RELEASED; lv_indev_read(s->input); return 0;
}
int bunbun_scene_direct_keys(bunbun_scene *s, uint32_t enabled) {
    if (enabled > 1) return -1;
    s->direct_keys = enabled != 0;
    if (s->direct_keys) lv_group_remove_all_objs(s->group);
    else for (unsigned i = 0; i < s->click_count; ++i) lv_group_add_obj(s->group, s->clicks[i].widget);
    if (s->runtime) bunbun_ui_set_key_callback(s->runtime, enabled ? dispatch_key : NULL, s);
    else lv_indev_set_group(s->input, enabled ? NULL : s->group);
    return 0;
}
int bunbun_scene_command_input(bunbun_scene *s) {
    if (!s || bunbun_scene_direct_keys(s, 1) != 0) return -1;
    s->command_input = true;
    return 0;
}
int bunbun_scene_input(bunbun_scene *s, uint32_t *key) {
    if (!s || !key || !s->command_input || s->overflow) return -1;
    if (!s->input_count) return 0;
    *key = s->input_keys[s->input_read];
    s->input_read = (s->input_read + 1) % 64;
    --s->input_count;
    return 1;
}
void bunbun_scene_extent(bunbun_scene *s, uint32_t *width, uint32_t *height) {
    if (!s || !width || !height) return;
    *width = (uint32_t)lv_display_get_horizontal_resolution(s->display);
    *height = (uint32_t)lv_display_get_vertical_resolution(s->display);
}
bunbun_widget *bunbun_widget_create(bunbun_scene *s, bunbun_widget *parent, uint32_t kind) {
    lv_obj_t *p = (lv_obj_t *)parent, *o = NULL;
    switch (kind) {
    case 0: o = lv_obj_create(p); break; case 1: o = lv_label_create(p); break;
    case 2: o = lv_bar_create(p); break; case 3: o = lv_button_create(p); break;
    case 4: o = lv_image_create(p); break;
    default: return NULL;
    }
    if (o && kind == 3 && !s->direct_keys) lv_group_add_obj(s->group, o);
    if (o) lv_obj_set_scrollable(o, false);
    return (bunbun_widget *)o;
}
void bunbun_widget_plain(bunbun_widget *w) { lv_obj_remove_style_all((lv_obj_t *)w); }
void bunbun_widget_size(bunbun_widget *w, int32_t width, int32_t height, uint32_t mask) {
    lv_obj_set_size((lv_obj_t *)w, mask & 1 ? LV_PCT(width) : width, mask & 2 ? LV_PCT(height) : height);
}
void bunbun_widget_width(bunbun_widget *w, int32_t width) { lv_obj_set_width((lv_obj_t *)w, width); }
int bunbun_widget_bounds(bunbun_widget *w, int32_t x, int32_t y, uint32_t width, uint32_t height) {
    if (!w || x < 0 || y < 0 || !width || !height || width > 4096 || height > 4096) return -1;
    lv_obj_t *o = (lv_obj_t *)w;
    lv_obj_set_layout(o, LV_LAYOUT_NONE);
    lv_obj_set_pos(o, x, y);
    lv_obj_set_size(o, (int32_t)width, (int32_t)height);
    return 0;
}
void bunbun_widget_flex(bunbun_widget *w, uint32_t column) {
    lv_obj_set_flex_flow((lv_obj_t *)w, column ? LV_FLEX_FLOW_COLUMN : LV_FLEX_FLOW_ROW);
}
static lv_flex_align_t align_value(uint32_t a) {
    return a == 2 ? LV_FLEX_ALIGN_SPACE_BETWEEN : a == 1 ? LV_FLEX_ALIGN_CENTER : LV_FLEX_ALIGN_START;
}
void bunbun_widget_align(bunbun_widget *w, uint32_t main, uint32_t cross, uint32_t track) {
    lv_obj_set_flex_align((lv_obj_t *)w, align_value(main), align_value(cross), align_value(track));
}
int bunbun_widget_style(bunbun_widget *w, uint32_t prop, uint32_t value, uint32_t selector) {
    lv_obj_t *o = (lv_obj_t *)w;
    if (selector > 3) return -1;
    lv_style_selector_t sel = selector == 1 ? LV_PART_INDICATOR : selector == 2 ? LV_STATE_FOCUSED :
                              selector == 3 ? LV_STATE_DISABLED : LV_PART_MAIN;
    switch (prop) {
    case 1: lv_obj_set_style_bg_color(o, lv_color_hex(value), sel); break;
    case 2: lv_obj_set_style_text_color(o, lv_color_hex(value), sel); break;
    case 3: lv_obj_set_style_border_color(o, lv_color_hex(value), sel); break;
    case 4: lv_obj_set_style_radius(o, (int32_t)value, sel); break;
    case 5: lv_obj_set_style_pad_all(o, (int32_t)value, sel); break;
    case 6: lv_obj_set_style_pad_row(o, (int32_t)value, sel); break;
    case 7: lv_obj_set_style_text_align(o, LV_TEXT_ALIGN_RIGHT, sel); break;
    case 8: lv_obj_set_style_bg_opa(o, (lv_opa_t)value, sel); break;
    case 9: lv_obj_set_style_outline_width(o, (int32_t)value, sel); break;
    case 10: lv_obj_set_style_outline_color(o, lv_color_hex(value), sel); break;
    case 11: lv_obj_set_style_border_width(o, (int32_t)value, sel); break;
    case 12: lv_obj_set_style_text_align(o, LV_TEXT_ALIGN_CENTER, sel); break;
    default: return -1;
    }
    return 0;
}
void bunbun_widget_font(bunbun_widget *w, const bunbun_font *f) {
    lv_obj_set_style_text_font((lv_obj_t *)w, (const lv_font_t *)f, 0);
}
void bunbun_widget_text(bunbun_widget *w, const char *text) { lv_label_set_text((lv_obj_t *)w, text); }
void bunbun_widget_center(bunbun_widget *w) { lv_obj_center((lv_obj_t *)w); }
void bunbun_widget_content_height(bunbun_widget *w) { lv_obj_set_height((lv_obj_t *)w, LV_SIZE_CONTENT); }
void bunbun_widget_enabled(bunbun_widget *w, uint32_t enabled) {
    if (enabled) lv_obj_remove_state((lv_obj_t *)w, LV_STATE_DISABLED);
    else lv_obj_add_state((lv_obj_t *)w, LV_STATE_DISABLED);
}
void bunbun_widget_hidden(bunbun_widget *w, uint32_t hidden) {
    lv_obj_set_hidden((lv_obj_t *)w, hidden != 0);
}
int bunbun_widget_image_rgba(bunbun_scene *s, bunbun_widget *w,
                            uint32_t width, uint32_t height, const uint8_t *rgba, size_t length) {
    if (!width || !height || width > 4096 || height > 4096 || !rgba ||
        length != (size_t)width * height * 4 || s->image_count == 16 ||
        !lv_obj_check_type((lv_obj_t *)w, &lv_image_class)) return -1;
    for (unsigned i = 0; i < s->image_count; ++i)
        if (s->images[i].widget == (lv_obj_t *)w) return -1;
    struct image_binding *b = &s->images[s->image_count];
    b->pixels = malloc(length); if (!b->pixels) return -1;
    for (size_t i = 0; i < length; i += 4) {
        b->pixels[i] = rgba[i+2]; b->pixels[i+1] = rgba[i+1];
        b->pixels[i+2] = rgba[i]; b->pixels[i+3] = rgba[i+3];
    }
    b->widget = (lv_obj_t *)w;
    b->descriptor.header.magic = LV_IMAGE_HEADER_MAGIC;
    b->descriptor.header.cf = LV_COLOR_FORMAT_ARGB8888;
    b->descriptor.header.w = width; b->descriptor.header.h = height;
    b->descriptor.header.stride = width * 4;
    b->descriptor.data_size = length; b->descriptor.data = b->pixels;
    ++s->image_count;
    lv_image_set_src(b->widget, &b->descriptor);
    uint32_t sx = (uint32_t)lv_display_get_horizontal_resolution(s->display) * 230 / width;
    uint32_t sy = (uint32_t)lv_display_get_vertical_resolution(s->display) * 230 / height;
    uint32_t scale = sx < sy ? sx : sy;
    if (!scale) scale = 1;
    lv_image_set_scale(b->widget, scale);
    lv_obj_center(b->widget);
    return 0;
}
int bunbun_widget_image_fit(bunbun_widget *w, uint32_t width, uint32_t height) {
    lv_obj_t *o = (lv_obj_t *)w;
    if (!o || !width || !height || width > 4096 || height > 4096 ||
        !lv_obj_check_type(o, &lv_image_class)) return -1;
    const lv_image_dsc_t *source = lv_image_get_src(o);
    if (!source || source->header.magic != LV_IMAGE_HEADER_MAGIC ||
        !source->header.w || !source->header.h) return -1;
    uint32_t sx = width * 256 / source->header.w;
    uint32_t sy = height * 256 / source->header.h;
    uint32_t scale = sx < sy ? sx : sy;
    lv_image_set_scale(o, scale ? scale : 1);
    lv_obj_center(o);
    return 0;
}
int bunbun_widget_bar(bunbun_widget *w, int32_t max, int32_t value) {
    if (max <= 0 || value < 0 || value > max) return -1;
    lv_bar_set_range((lv_obj_t *)w, 0, max); lv_bar_set_value((lv_obj_t *)w, value, LV_ANIM_OFF); return 0;
}
static void clicked(lv_event_t *event) {
    struct click_binding *b = lv_event_get_user_data(event); bunbun_scene *s = b->scene;
    if (s->count == 64) { s->overflow = true; return; }
    s->actions[s->write] = b->action; s->write = (s->write + 1) % 64; ++s->count;
}
int bunbun_widget_on_click(bunbun_scene *s, bunbun_widget *w, uint32_t action) {
    if (!action || s->click_count == 128) return -1;
    struct click_binding *b = &s->clicks[s->click_count]; b->scene = s; b->action = action; b->widget = (lv_obj_t *)w;
    if (!lv_obj_add_event_cb((lv_obj_t *)w, clicked, LV_EVENT_CLICKED, b)) return -1;
    ++s->click_count; return 0;
}
int bunbun_widget_shortcut(bunbun_scene *s, bunbun_widget *w, uint32_t key) {
    key = command_key(key);
    if (!key || s->shortcut_count == 128 || !lv_obj_check_type((lv_obj_t *)w, &lv_button_class)) return -1;
    for (unsigned i = 0; i < s->shortcut_count; ++i)
        if (s->shortcuts[i].key == key) return -1;
    s->shortcuts[s->shortcut_count++] = (struct key_binding){(lv_obj_t *)w, key};
    return 0;
}
