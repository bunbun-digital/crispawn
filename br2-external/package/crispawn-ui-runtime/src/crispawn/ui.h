// SPDX-License-Identifier: Apache-2.0
#ifndef CRISPAWN_UI_H
#define CRISPAWN_UI_H

/* Portable command keys, separate from LVGL and Linux input constants. */
#define CRISPAWN_KEY_PAGE_UP 0x110001u
#define CRISPAWN_KEY_PAGE_DOWN 0x110002u
#define CRISPAWN_KEY_END 0x110003u

#include <stdint.h>
#include <lvgl/lvgl.h>

#ifdef __cplusplus
extern "C" {
#endif

#define CRISPAWN_UI_API_VERSION 1
typedef struct crispawn_ui crispawn_ui;

/* One foreground UI per system. NULL means initialization failed (see stderr). */
crispawn_ui *crispawn_ui_open(void);
crispawn_ui *crispawn_ui_open_display_only(void);
lv_display_t *crispawn_ui_display(crispawn_ui *ui);
lv_group_t *crispawn_ui_group(crispawn_ui *ui);
const lv_font_t *crispawn_ui_font(crispawn_ui *ui);
/* The OS default font at a chosen pixel size; owned by ui until close. */
const lv_font_t *crispawn_ui_font_size(crispawn_ui *ui, uint32_t pixels);
unsigned crispawn_ui_keyboard_count(crispawn_ui *ui);
uint64_t crispawn_ui_input_sequence(crispawn_ui *ui);
/* Press edges only, ASCII letters/digits where supported. A callback selects
 * direct-command mode instead of LVGL focus navigation; NULL restores it. */
void crispawn_ui_set_key_callback(crispawn_ui *ui, void (*callback)(void *, uint32_t), void *data);
/* Process events on the calling thread; return suggested sleep in milliseconds. */
uint32_t crispawn_ui_step(crispawn_ui *ui);
void crispawn_ui_close(crispawn_ui *ui);

#ifdef __cplusplus
}
#endif
#endif
