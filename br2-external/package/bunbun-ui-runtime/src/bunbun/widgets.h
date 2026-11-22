// SPDX-License-Identifier: Apache-2.0
/* SPDX-License-Identifier: LicenseRef-Bunbun-Proprietary */
#ifndef BUNBUN_WIDGETS_H
#define BUNBUN_WIDGETS_H
#include <stddef.h>
#include <stdint.h>

/* Additive opaque API, independent of LVGL struct/enum layout. UI thread only.
 * Widgets/fonts belong to the scene until close; do not use legacy LVGL APIs
 * to delete them or initialize a concurrent LVGL session. */
typedef struct bunbun_scene bunbun_scene;
typedef struct bunbun_widget bunbun_widget;
typedef struct bunbun_font bunbun_font;
uint32_t bunbun_widgets_api_version(void);
bunbun_scene *bunbun_scene_open(void);
/* Allows display before an evdev keyboard has enumerated; hotplug is retried. */
bunbun_scene *bunbun_scene_open_display_only(void);
/* Explicit development mode: no framebuffer, console or input device access. */
bunbun_scene *bunbun_scene_open_memory(uint32_t width, uint32_t height, const char *font);
void bunbun_scene_close(bunbun_scene *scene);
uint32_t bunbun_scene_step(bunbun_scene *scene);
uint32_t bunbun_scene_keyboards(bunbun_scene *scene);
uint64_t bunbun_scene_input_sequence(bunbun_scene *scene);
uint32_t bunbun_scene_should_quit(bunbun_scene *scene);
int bunbun_scene_action(bunbun_scene *scene, uint32_t *action);
bunbun_widget *bunbun_scene_root(bunbun_scene *scene);
bunbun_font *bunbun_scene_font(bunbun_scene *scene, uint32_t pixels);
int bunbun_scene_theme(bunbun_scene *scene, uint32_t primary, uint32_t secondary, const bunbun_font *font);
int bunbun_scene_render_rgb(bunbun_scene *scene, uint8_t *rgb, size_t length);
/* Memory mode only: 1=next, 2=previous, 3=enter, ASCII letters/digits.
 * Actual LVGL keypad dispatch, including press/release edges. */
int bunbun_scene_send_key(bunbun_scene *scene, uint32_t key);
/* Direct-command mode disables focus navigation and Enter activation. */
int bunbun_scene_direct_keys(bunbun_scene *scene, uint32_t enabled);
/* Commands for a product-owned state machine. Does not click bound buttons.
 * ASCII letters/digits, Escape=27, or BUNBUN_KEY_*; arrows/Enter are ignored.
 * Input queue overflow is an error rather than a fabricated command. */
int bunbun_scene_command_input(bunbun_scene *scene);
int bunbun_scene_input(bunbun_scene *scene, uint32_t *key);
void bunbun_scene_extent(bunbun_scene *scene, uint32_t *width, uint32_t *height);
/* kind: 0=container, 1=label, 2=bar, 3=button, 4=image. */
bunbun_widget *bunbun_widget_create(bunbun_scene *scene, bunbun_widget *parent, uint32_t kind);
void bunbun_widget_plain(bunbun_widget *widget);
/* Percentage dimensions are separate from pixel values, not encoded LVGL constants. */
void bunbun_widget_size(bunbun_widget *widget, int32_t width, int32_t height, uint32_t percent_mask);
void bunbun_widget_width(bunbun_widget *widget, int32_t width);
/* Pixel layout, with no flex layout on this object. */
int bunbun_widget_bounds(bunbun_widget *widget, int32_t x, int32_t y, uint32_t width, uint32_t height);
void bunbun_widget_flex(bunbun_widget *widget, uint32_t column);
/* align: 0=start, 1=center, 2=space between. */
void bunbun_widget_align(bunbun_widget *widget, uint32_t main, uint32_t cross, uint32_t track);
/* selector: 0=main, 1=indicator, 2=focused, 3=disabled.
 * property: 1=background, 2=text color, 3=border color, 4=radius,
 * 5=padding all, 6=padding row, 7=text right align, 8=background opacity,
 * 9=outline width, 10=outline color, 11=border width, 12=text center. */
int bunbun_widget_style(bunbun_widget *widget, uint32_t property, uint32_t value, uint32_t selector);
void bunbun_widget_font(bunbun_widget *widget, const bunbun_font *font);
void bunbun_widget_text(bunbun_widget *widget, const char *text);
void bunbun_widget_center(bunbun_widget *widget);
/* Intrinsic height for vertically centered one/two-line button captions. */
void bunbun_widget_content_height(bunbun_widget *widget);
void bunbun_widget_enabled(bunbun_widget *widget, uint32_t enabled);
void bunbun_widget_hidden(bunbun_widget *widget, uint32_t hidden);
/* Copies RGBA bytes, retained by the scene. One image per widget. Uniformly
 * scales to fit 90% of the display, centered. No image codec dependency. */
int bunbun_widget_image_rgba(bunbun_scene *scene, bunbun_widget *widget,
                            uint32_t width, uint32_t height, const uint8_t *rgba, size_t length);
/* Uniformly fit an existing image into a product-owned box. */
int bunbun_widget_image_fit(bunbun_widget *widget, uint32_t width, uint32_t height);
int bunbun_widget_bar(bunbun_widget *widget, int32_t maximum, int32_t value);
int bunbun_widget_on_click(bunbun_scene *scene, bunbun_widget *widget, uint32_t action);
/* Case-insensitive ASCII letter/digit. Disabled/hidden buttons do not dispatch.
 * Duplicate keys are rejected. The button's normal click action is reused. */
int bunbun_widget_shortcut(bunbun_scene *scene, bunbun_widget *widget, uint32_t key);
#endif
