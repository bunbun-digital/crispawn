// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: LicenseRef-Bunbun-Proprietary
use std::ffi::c_char;
#[repr(C)]
pub struct Scene {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct Object {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct Font {
    _opaque: [u8; 0],
}
unsafe extern "C" {
    pub fn bunbun_widgets_api_version() -> u32;
    pub fn bunbun_scene_open() -> *mut Scene;
    pub fn bunbun_scene_open_display_only() -> *mut Scene;
    pub fn bunbun_scene_open_memory(w: u32, h: u32, font: *const c_char) -> *mut Scene;
    pub fn bunbun_scene_close(s: *mut Scene);
    pub fn bunbun_scene_step(s: *mut Scene) -> u32;
    pub fn bunbun_scene_keyboards(s: *mut Scene) -> u32;
    pub fn bunbun_scene_input_sequence(s: *mut Scene) -> u64;
    pub fn bunbun_scene_should_quit(s: *mut Scene) -> u32;
    pub fn bunbun_scene_action(s: *mut Scene, action: *mut u32) -> i32;
    pub fn bunbun_scene_root(s: *mut Scene) -> *mut Object;
    pub fn bunbun_scene_font(s: *mut Scene, pixels: u32) -> *mut Font;
    pub fn bunbun_scene_theme(
        s: *mut Scene,
        primary: u32,
        secondary: u32,
        font: *const Font,
    ) -> i32;
    pub fn bunbun_scene_render_rgb(s: *mut Scene, rgb: *mut u8, size: usize) -> i32;
    pub fn bunbun_scene_send_key(s: *mut Scene, key: u32) -> i32;
    pub fn bunbun_scene_direct_keys(s: *mut Scene, enabled: u32) -> i32;
    pub fn bunbun_scene_command_input(s: *mut Scene) -> i32;
    pub fn bunbun_scene_input(s: *mut Scene, key: *mut u32) -> i32;
    pub fn bunbun_scene_extent(s: *mut Scene, width: *mut u32, height: *mut u32);
    pub fn bunbun_widget_create(s: *mut Scene, p: *mut Object, kind: u32) -> *mut Object;
    pub fn bunbun_widget_plain(o: *mut Object);
    pub fn bunbun_widget_size(o: *mut Object, w: i32, h: i32, mask: u32);
    pub fn bunbun_widget_width(o: *mut Object, w: i32);
    pub fn bunbun_widget_bounds(o: *mut Object, x: i32, y: i32, w: u32, h: u32) -> i32;
    pub fn bunbun_widget_flex(o: *mut Object, column: u32);
    pub fn bunbun_widget_align(o: *mut Object, main: u32, cross: u32, track: u32);
    pub fn bunbun_widget_style(o: *mut Object, prop: u32, value: u32, selector: u32) -> i32;
    pub fn bunbun_widget_font(o: *mut Object, font: *const Font);
    pub fn bunbun_widget_text(o: *mut Object, text: *const c_char);
    pub fn bunbun_widget_center(o: *mut Object);
    pub fn bunbun_widget_content_height(o: *mut Object);
    pub fn bunbun_widget_enabled(o: *mut Object, enabled: u32);
    pub fn bunbun_widget_hidden(o: *mut Object, hidden: u32);
    pub fn bunbun_widget_image_rgba(
        s: *mut Scene,
        o: *mut Object,
        w: u32,
        h: u32,
        rgba: *const u8,
        size: usize,
    ) -> i32;
    pub fn bunbun_widget_image_fit(o: *mut Object, w: u32, h: u32) -> i32;
    pub fn bunbun_widget_bar(o: *mut Object, max: i32, value: i32) -> i32;
    pub fn bunbun_widget_on_click(s: *mut Scene, o: *mut Object, action: u32) -> i32;
    pub fn bunbun_widget_shortcut(s: *mut Scene, o: *mut Object, key: u32) -> i32;
}
