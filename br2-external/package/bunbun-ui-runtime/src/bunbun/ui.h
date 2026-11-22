// SPDX-License-Identifier: Apache-2.0
/* SPDX-License-Identifier: LicenseRef-Bunbun-Proprietary */
#ifndef BUNBUN_UI_H
#define BUNBUN_UI_H

/* Portable command keys, separate from LVGL and Linux input constants. */
#define BUNBUN_KEY_PAGE_UP 0x110001u
#define BUNBUN_KEY_PAGE_DOWN 0x110002u
#define BUNBUN_KEY_END 0x110003u

#include <stdint.h>
#include <lvgl/lvgl.h>

#ifdef __cplusplus
extern "C" {
#endif

#define BUNBUN_UI_API_VERSION 1
typedef struct bunbun_ui bunbun_ui;

/* One foreground UI per system. NULL means initialization failed (see stderr). */
bunbun_ui *bunbun_ui_open(void);
bunbun_ui *bunbun_ui_open_display_only(void);
lv_display_t *bunbun_ui_display(bunbun_ui *ui);
lv_group_t *bunbun_ui_group(bunbun_ui *ui);
const lv_font_t *bunbun_ui_font(bunbun_ui *ui);
/* The OS default font at a chosen pixel size; owned by ui until close. */
const lv_font_t *bunbun_ui_font_size(bunbun_ui *ui, uint32_t pixels);
unsigned bunbun_ui_keyboard_count(bunbun_ui *ui);
uint64_t bunbun_ui_input_sequence(bunbun_ui *ui);
/* Press edges only, ASCII letters/digits where supported. A callback selects
 * direct-command mode instead of LVGL focus navigation; NULL restores it. */
void bunbun_ui_set_key_callback(bunbun_ui *ui, void (*callback)(void *, uint32_t), void *data);
/* Process events on the calling thread; return suggested sleep in milliseconds. */
uint32_t bunbun_ui_step(bunbun_ui *ui);
void bunbun_ui_close(bunbun_ui *ui);

#ifdef __cplusplus
}
#endif
#endif
