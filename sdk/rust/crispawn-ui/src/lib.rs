// SPDX-License-Identifier: Apache-2.0
//! Single-threaded, scene-owned handles over the OS C ABI. No LVGL structs or
//! raw pointers cross the safe API. Objects cannot be deleted independently.
#![deny(unsafe_op_in_unsafe_fn)]
mod ffi;
use std::ffi::CString;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

static OWNED: AtomicBool = AtomicBool::new(false);
#[derive(Debug)]
pub struct Error(&'static str);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
fn checked(value: i32) -> Result<()> {
    if value < 0 {
        Err(Error("OS UI operation failed"))
    } else {
        Ok(())
    }
}
fn string(text: &str) -> Result<CString> {
    CString::new(text).map_err(|_| Error("text contains an interior NUL"))
}
fn shortcut(key: char) -> Result<u32> {
    if !key.is_ascii_alphanumeric() {
        return Err(Error("shortcut must be an ASCII letter or digit"));
    }
    Ok(key.to_ascii_lowercase() as u32)
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    Container,
    Label,
    Bar,
    Button,
    Image,
}
#[derive(Clone, Copy)]
pub enum Length {
    Pixels(u16),
    Percent(u8),
}
impl Length {
    fn parts(self) -> Result<(i32, u32)> {
        match self {
            Self::Pixels(v) => Ok((v.into(), 0)),
            Self::Percent(v) if v <= 100 => Ok((v.into(), 1)),
            _ => Err(Error("percentage must be in 0..=100")),
        }
    }
}
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum Align {
    Start,
    Center,
    SpaceBetween,
}
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum Selector {
    Main,
    Indicator,
    Focused,
    Disabled,
}
pub enum Style {
    Background(u32),
    Text(u32),
    Border(u32),
    Radius(u16),
    Padding(u16),
    RowGap(u16),
    TextRight,
    Opacity(u8),
    Outline(u16),
    OutlineColor(u32),
    BorderWidth(u16),
    TextCenter,
}
impl Style {
    fn parts(self) -> (u32, u32) {
        match self {
            Self::Background(v) => (1, v),
            Self::Text(v) => (2, v),
            Self::Border(v) => (3, v),
            Self::Radius(v) => (4, v.into()),
            Self::Padding(v) => (5, v.into()),
            Self::RowGap(v) => (6, v.into()),
            Self::TextRight => (7, 0),
            Self::Opacity(v) => (8, v.into()),
            Self::Outline(v) => (9, v.into()),
            Self::OutlineColor(v) => (10, v),
            Self::BorderWidth(v) => (11, v.into()),
            Self::TextCenter => (12, 0),
        }
    }
}
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum Key {
    Next = 1,
    Previous = 2,
    Enter = 3,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKey {
    Character(char),
    Escape,
    PageUp,
    PageDown,
    End,
}
impl InputKey {
    fn code(self) -> Result<u32> {
        match self {
            Self::Character(c) => shortcut(c),
            Self::Escape => Ok(27),
            Self::PageUp => Ok(0x110001),
            Self::PageDown => Ok(0x110002),
            Self::End => Ok(0x110003),
        }
    }
}
pub struct Ui {
    raw: NonNull<ffi::Scene>,
    memory: Option<(u32, u32)>,
    _thread: PhantomData<Rc<()>>,
}
pub struct Font<'ui> {
    raw: NonNull<ffi::Font>,
    owner: &'ui Ui,
}
#[derive(Clone, Copy)]
pub struct Widget<'ui> {
    raw: NonNull<ffi::Object>,
    owner: &'ui Ui,
    kind: Kind,
}

impl Ui {
    fn create(memory: Option<(u32, u32, &str)>, display_only: bool) -> Result<Self> {
        if let Some((w, h, _)) = memory {
            if w == 0 || h == 0 || w > 4096 || h > 4096 {
                return Err(Error("invalid memory display size"));
            }
        }
        let font = memory.map(|(_, _, s)| string(s)).transpose()?;
        if OWNED
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(Error("only one UI scene may be open"));
        }
        // SAFETY: the wrapper exclusively owns the process UI session; strings
        // remain valid during the synchronous factory call. API version is fixed.
        let raw = unsafe {
            if ffi::crispawn_widgets_api_version() != 1 {
                std::ptr::null_mut()
            } else if let Some((w, h, _)) = memory {
                ffi::crispawn_scene_open_memory(w, h, font.as_ref().unwrap().as_ptr())
            } else if display_only {
                ffi::crispawn_scene_open_display_only()
            } else {
                ffi::crispawn_scene_open()
            }
        };
        match NonNull::new(raw) {
            Some(raw) => Ok(Self {
                raw,
                memory: memory.map(|(w, h, _)| (w, h)),
                _thread: PhantomData,
            }),
            None => {
                OWNED.store(false, Ordering::Release);
                Err(Error("OS UI initialization failed"))
            }
        }
    }
    pub fn open() -> Result<Self> {
        Self::create(None, false)
    }
    /// Draw during device enumeration; keyboards attach when available.
    pub fn display_only() -> Result<Self> {
        Self::create(None, true)
    }
    pub fn memory(width: u32, height: u32, font_path: &str) -> Result<Self> {
        Self::create(Some((width, height, font_path)), false)
    }
    pub fn root(&self) -> Result<Widget<'_>> {
        // SAFETY: the scene is live; returned root is owned until scene close.
        let raw = NonNull::new(unsafe { ffi::crispawn_scene_root(self.raw.as_ptr()) })
            .ok_or(Error("missing root widget"))?;
        Ok(Widget {
            raw,
            owner: self,
            kind: Kind::Container,
        })
    }
    pub fn child<'a>(&'a self, parent: Widget<'a>, kind: Kind) -> Result<Widget<'a>> {
        if !std::ptr::eq(self, parent.owner) {
            return Err(Error("widget belongs to a different UI"));
        }
        // SAFETY: parent belongs to this scene and cannot be deleted through the safe API.
        let raw = NonNull::new(unsafe {
            ffi::crispawn_widget_create(self.raw.as_ptr(), parent.raw.as_ptr(), kind as u32)
        })
        .ok_or(Error("widget allocation failed"))?;
        Ok(Widget {
            raw,
            owner: self,
            kind,
        })
    }
    pub fn font(&self, pixels: u32) -> Result<Font<'_>> {
        // SAFETY: scene owns and caches fonts until close; null is checked.
        let raw = NonNull::new(unsafe { ffi::crispawn_scene_font(self.raw.as_ptr(), pixels) })
            .ok_or(Error("font load failed"))?;
        Ok(Font { raw, owner: self })
    }
    pub fn theme(&self, primary: u32, secondary: u32, font: &Font<'_>) -> Result<()> {
        if !std::ptr::eq(self, font.owner) {
            return Err(Error("font belongs to a different UI"));
        }
        // SAFETY: live scene and a font it owns.
        checked(unsafe {
            ffi::crispawn_scene_theme(self.raw.as_ptr(), primary, secondary, font.raw.as_ptr())
        })
    }
    pub fn step(&self) -> u32 {
        unsafe { ffi::crispawn_scene_step(self.raw.as_ptr()) }
    }
    pub fn keyboards(&self) -> u32 {
        unsafe { ffi::crispawn_scene_keyboards(self.raw.as_ptr()) }
    }
    pub fn input_sequence(&self) -> u64 {
        unsafe { ffi::crispawn_scene_input_sequence(self.raw.as_ptr()) }
    }
    pub fn should_quit(&self) -> bool {
        unsafe { ffi::crispawn_scene_should_quit(self.raw.as_ptr()) != 0 }
    }
    pub fn next_action(&self) -> Result<Option<u32>> {
        let mut action = 0;
        // SAFETY: scene is live and action points to one writable u32.
        match unsafe { ffi::crispawn_scene_action(self.raw.as_ptr(), &mut action) } {
            0 => Ok(None),
            1 => Ok(Some(action)),
            _ => Err(Error("UI action queue overflow")),
        }
    }
    pub fn send_key(&self, key: Key) -> Result<()> {
        // SAFETY: key enum is validated by construction; C rejects live mode.
        checked(unsafe { ffi::crispawn_scene_send_key(self.raw.as_ptr(), key as u32) })
    }
    /// Use button shortcuts without a focus cursor, Tab, arrows or Enter.
    pub fn direct_keys(&self) -> Result<()> {
        // SAFETY: the scene is live and all input runs on its owning thread.
        checked(unsafe { ffi::crispawn_scene_direct_keys(self.raw.as_ptr(), 1) })
    }
    /// Development-only injection through the same LVGL press/release path.
    pub fn send_shortcut(&self, key: char) -> Result<()> {
        let key = shortcut(key)?;
        // SAFETY: validated ASCII key; C rejects live scenes.
        checked(unsafe { ffi::crispawn_scene_send_key(self.raw.as_ptr(), key) })
    }
    pub fn render_rgb(&self) -> Result<Vec<u8>> {
        let (w, h) = self.memory.ok_or(Error("snapshot requires memory mode"))?;
        let mut out = vec![0; (w as usize) * (h as usize) * 3];
        // SAFETY: geometry was bounded at creation; the allocation covers the exact RGB frame.
        checked(unsafe {
            ffi::crispawn_scene_render_rgb(self.raw.as_ptr(), out.as_mut_ptr(), out.len())
        })?;
        Ok(out)
    }
    /// Product-owned keyboard state machines, including modal confirmation.
    pub fn command_input(&self) -> Result<()> {
        // SAFETY: this thread owns the live scene and its input queues.
        checked(unsafe { ffi::crispawn_scene_command_input(self.raw.as_ptr()) })
    }
    pub fn next_input(&self) -> Result<Option<InputKey>> {
        let mut key = 0;
        // SAFETY: a writable u32 is provided for the synchronous queue read.
        match unsafe { ffi::crispawn_scene_input(self.raw.as_ptr(), &mut key) } {
            0 => Ok(None),
            1 => Ok(Some(match key {
                27 => InputKey::Escape,
                0x110001 => InputKey::PageUp,
                0x110002 => InputKey::PageDown,
                0x110003 => InputKey::End,
                n if n < 128 && (n as u8).is_ascii_alphanumeric() => {
                    InputKey::Character(n as u8 as char)
                }
                _ => return Err(Error("unknown command key")),
            })),
            _ => Err(Error("UI input queue overflow or wrong input mode")),
        }
    }
    pub fn send_input(&self, key: InputKey) -> Result<()> {
        // SAFETY: only bounded portable key codes; C rejects physical displays.
        checked(unsafe { ffi::crispawn_scene_send_key(self.raw.as_ptr(), key.code()?) })
    }
    pub fn extent(&self) -> (u32, u32) {
        let (mut width, mut height) = (0, 0);
        // SAFETY: two live writable u32s; scene owns its display.
        unsafe { ffi::crispawn_scene_extent(self.raw.as_ptr(), &mut width, &mut height) };
        (width, height)
    }
}
impl Drop for Ui {
    fn drop(&mut self) {
        // SAFETY: unique scene ownership; borrowed handles cannot outlive self.
        unsafe { ffi::crispawn_scene_close(self.raw.as_ptr()) };
        OWNED.store(false, Ordering::Release);
    }
}
impl Widget<'_> {
    fn require(&self, kind: Kind) -> Result<()> {
        if self.kind == kind {
            Ok(())
        } else {
            Err(Error("wrong widget kind"))
        }
    }
    // All pointers below are scene-owned. Safe callers cannot delete objects,
    // move them to another thread, or use a handle after the Ui borrow ends.
    pub fn plain(self) {
        unsafe { ffi::crispawn_widget_plain(self.raw.as_ptr()) }
    }
    pub fn size(self, width: Length, height: Length) -> Result<()> {
        let (w, a) = width.parts()?;
        let (h, b) = height.parts()?;
        unsafe { ffi::crispawn_widget_size(self.raw.as_ptr(), w, h, a | (b << 1)) };
        Ok(())
    }
    pub fn width(self, width: u16) {
        unsafe { ffi::crispawn_widget_width(self.raw.as_ptr(), width.into()) }
    }
    pub fn flex(self, column: bool) {
        unsafe { ffi::crispawn_widget_flex(self.raw.as_ptr(), column.into()) }
    }
    pub fn align(self, main: Align, cross: Align, track: Align) {
        unsafe {
            ffi::crispawn_widget_align(self.raw.as_ptr(), main as u32, cross as u32, track as u32)
        }
    }
    pub fn style(self, style: Style, selector: Selector) -> Result<()> {
        let (property, value) = style.parts();
        checked(unsafe {
            ffi::crispawn_widget_style(self.raw.as_ptr(), property, value, selector as u32)
        })
    }
    pub fn font(self, font: &Font<'_>) -> Result<()> {
        if !std::ptr::eq(self.owner, font.owner) {
            return Err(Error("font belongs to a different UI"));
        }
        unsafe { ffi::crispawn_widget_font(self.raw.as_ptr(), font.raw.as_ptr()) };
        Ok(())
    }
    pub fn text(self, value: &str) -> Result<()> {
        self.require(Kind::Label)?;
        let value = string(value)?;
        // lv_label_set_text copies the bytes during this call.
        unsafe { ffi::crispawn_widget_text(self.raw.as_ptr(), value.as_ptr()) };
        Ok(())
    }
    pub fn center(self) {
        unsafe { ffi::crispawn_widget_center(self.raw.as_ptr()) }
    }
    pub fn content_height(self) {
        unsafe { ffi::crispawn_widget_content_height(self.raw.as_ptr()) }
    }
    pub fn bounds(self, x: u16, y: u16, width: u16, height: u16) -> Result<()> {
        // SAFETY: live widget with bounded pixel geometry; C validates extents.
        checked(unsafe {
            ffi::crispawn_widget_bounds(
                self.raw.as_ptr(),
                x.into(),
                y.into(),
                width.into(),
                height.into(),
            )
        })
    }
    pub fn enabled(self, enabled: bool) {
        unsafe { ffi::crispawn_widget_enabled(self.raw.as_ptr(), enabled.into()) }
    }
    pub fn hidden(self, hidden: bool) {
        // SAFETY: scene owns the widget for this handle's lifetime.
        unsafe { ffi::crispawn_widget_hidden(self.raw.as_ptr(), hidden.into()) }
    }
    /// Copy an RGBA image into scene-owned storage, centered and aspect-fitted.
    pub fn image_rgba(self, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        self.require(Kind::Image)?;
        if width == 0
            || height == 0
            || width > 4096
            || height > 4096
            || rgba.len() != width as usize * height as usize * 4
        {
            return Err(Error("invalid RGBA image dimensions"));
        }
        // SAFETY: the validated slice is borrowed for the call and C copies it.
        checked(unsafe {
            ffi::crispawn_widget_image_rgba(
                self.owner.raw.as_ptr(),
                self.raw.as_ptr(),
                width,
                height,
                rgba.as_ptr(),
                rgba.len(),
            )
        })
    }
    pub fn bar(self, maximum: i32, value: i32) -> Result<()> {
        self.require(Kind::Bar)?;
        checked(unsafe { ffi::crispawn_widget_bar(self.raw.as_ptr(), maximum, value) })
    }
    pub fn image_fit(self, width: u16, height: u16) -> Result<()> {
        self.require(Kind::Image)?;
        // SAFETY: the scene owns the descriptor and pixels loaded by image_rgba.
        checked(unsafe {
            ffi::crispawn_widget_image_fit(self.raw.as_ptr(), width.into(), height.into())
        })
    }
    pub fn on_click(self, action: u32) -> Result<()> {
        self.require(Kind::Button)?;
        checked(unsafe {
            ffi::crispawn_widget_on_click(self.owner.raw.as_ptr(), self.raw.as_ptr(), action)
        })
    }
    /// Bind a case-insensitive key to this button's click action.
    pub fn shortcut(self, key: char) -> Result<()> {
        self.require(Kind::Button)?;
        let key = shortcut(key)?;
        // SAFETY: both handles belong to the same live scene. C rejects duplicate keys.
        checked(unsafe {
            ffi::crispawn_widget_shortcut(self.owner.raw.as_ptr(), self.raw.as_ptr(), key)
        })
    }
}
