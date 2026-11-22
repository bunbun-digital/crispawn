// SPDX-License-Identifier: Apache-2.0
#ifndef CRISPAWN_WIDGETS_H
#define CRISPAWN_WIDGETS_H
#include <stddef.h>
#include <stdint.h>

/* Additive opaque API, independent of LVGL struct/enum layout. UI thread only.
 * Widgets/fonts belong to the scene until close; do not use legacy LVGL APIs
 * to delete them or initialize a concurrent LVGL session. */
typedef struct crispawn_scene crispawn_scene;
typedef struct crispawn_widget crispawn_widget;
typedef struct crispawn_font crispawn_font;
uint32_t crispawn_widgets_api_version(void);
crispawn_scene *crispawn_scene_open(void);
/* Allows display before an evdev keyboard has enumerated; hotplug is retried. */
crispawn_scene *crispawn_scene_open_display_only(void);
/* Explicit development mode: no framebuffer, console or input device access. */
crispawn_scene *crispawn_scene_open_memory(uint32_t width, uint32_t height, const char *font);
void crispawn_scene_close(crispawn_scene *scene);
uint32_t crispawn_scene_step(crispawn_scene *scene);
uint32_t crispawn_scene_keyboards(crispawn_scene *scene);
uint64_t crispawn_scene_input_sequence(crispawn_scene *scene);
uint32_t crispawn_scene_should_quit(crispawn_scene *scene);
int crispawn_scene_action(crispawn_scene *scene, uint32_t *action);
crispawn_widget *crispawn_scene_root(crispawn_scene *scene);
crispawn_font *crispawn_scene_font(crispawn_scene *scene, uint32_t pixels);
int crispawn_scene_theme(crispawn_scene *scene, uint32_t primary, uint32_t secondary, const crispawn_font *font);
int crispawn_scene_render_rgb(crispawn_scene *scene, uint8_t *rgb, size_t length);
/* Memory mode only: 1=next, 2=previous, 3=enter, ASCII letters/digits.
 * Actual LVGL keypad dispatch, including press/release edges. */
int crispawn_scene_send_key(crispawn_scene *scene, uint32_t key);
/* Direct-command mode disables focus navigation and Enter activation. */
int crispawn_scene_direct_keys(crispawn_scene *scene, uint32_t enabled);
/* Commands for a product-owned state machine. Does not click bound buttons.
 * ASCII letters/digits, Escape=27, or CRISPAWN_KEY_*; arrows/Enter are ignored.
 * Input queue overflow is an error rather than a fabricated command. */
int crispawn_scene_command_input(crispawn_scene *scene);
int crispawn_scene_input(crispawn_scene *scene, uint32_t *key);
void crispawn_scene_extent(crispawn_scene *scene, uint32_t *width, uint32_t *height);
/* kind: 0=container, 1=label, 2=bar, 3=button, 4=image. */
crispawn_widget *crispawn_widget_create(crispawn_scene *scene, crispawn_widget *parent, uint32_t kind);
void crispawn_widget_plain(crispawn_widget *widget);
/* Percentage dimensions are separate from pixel values, not encoded LVGL constants. */
void crispawn_widget_size(crispawn_widget *widget, int32_t width, int32_t height, uint32_t percent_mask);
void crispawn_widget_width(crispawn_widget *widget, int32_t width);
/* Pixel layout, with no flex layout on this object. */
int crispawn_widget_bounds(crispawn_widget *widget, int32_t x, int32_t y, uint32_t width, uint32_t height);
void crispawn_widget_flex(crispawn_widget *widget, uint32_t column);
/* align: 0=start, 1=center, 2=space between. */
void crispawn_widget_align(crispawn_widget *widget, uint32_t main, uint32_t cross, uint32_t track);
/* selector: 0=main, 1=indicator, 2=focused, 3=disabled.
 * property: 1=background, 2=text color, 3=border color, 4=radius,
 * 5=padding all, 6=padding row, 7=text right align, 8=background opacity,
 * 9=outline width, 10=outline color, 11=border width, 12=text center. */
int crispawn_widget_style(crispawn_widget *widget, uint32_t property, uint32_t value, uint32_t selector);
void crispawn_widget_font(crispawn_widget *widget, const crispawn_font *font);
void crispawn_widget_text(crispawn_widget *widget, const char *text);
void crispawn_widget_center(crispawn_widget *widget);
/* Intrinsic height for vertically centered one/two-line button captions. */
void crispawn_widget_content_height(crispawn_widget *widget);
void crispawn_widget_enabled(crispawn_widget *widget, uint32_t enabled);
void crispawn_widget_hidden(crispawn_widget *widget, uint32_t hidden);
/* Copies RGBA bytes, retained by the scene. One image per widget. Uniformly
 * scales to fit 90% of the display, centered. No image codec dependency. */
int crispawn_widget_image_rgba(crispawn_scene *scene, crispawn_widget *widget,
                            uint32_t width, uint32_t height, const uint8_t *rgba, size_t length);
/* Uniformly fit an existing image into a product-owned box. */
int crispawn_widget_image_fit(crispawn_widget *widget, uint32_t width, uint32_t height);
int crispawn_widget_bar(crispawn_widget *widget, int32_t maximum, int32_t value);
int crispawn_widget_on_click(crispawn_scene *scene, crispawn_widget *widget, uint32_t action);
/* Case-insensitive ASCII letter/digit. Disabled/hidden buttons do not dispatch.
 * Duplicate keys are rejected. The button's normal click action is reused. */
int crispawn_widget_shortcut(crispawn_scene *scene, crispawn_widget *widget, uint32_t key);
#endif
