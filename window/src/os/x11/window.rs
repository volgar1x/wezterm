use super::*;
use crate::bitmaps::*;
use crate::{
    Color, Dimensions, KeyEvent, MouseButtons, MouseEvent, MouseEventKind, MousePress, Operator,
    PaintContext, WindowCallbacks,
};
use failure::Fallible;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

fn value_in_range(value: u16, min: u16, max: u16) -> bool {
    value >= min && value <= max
}

impl Rect {
    fn right(&self) -> u16 {
        self.x + self.width
    }

    fn bottom(&self) -> u16 {
        self.y + self.height
    }

    fn enclosing_boundary_with(&self, other: &Rect) -> Self {
        let left = self.x.min(other.x);
        let right = self.right().max(other.right());

        let top = self.y.min(other.y);
        let bottom = self.bottom().max(other.bottom());

        Self {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
    }

    // https://stackoverflow.com/a/306379/149111
    fn intersects_with(&self, other: &Rect) -> bool {
        let x_overlaps = value_in_range(self.x, other.x, other.right())
            || value_in_range(other.x, self.x, self.right());

        let y_overlaps = value_in_range(self.y, other.y, other.bottom())
            || value_in_range(other.y, self.x, self.bottom());

        x_overlaps && y_overlaps
    }
}

struct WindowInner {
    window_id: xcb::xproto::Window,
    conn: Arc<Connection>,
    callbacks: Box<WindowCallbacks>,
    window_context: Context,
    width: u16,
    height: u16,
    expose: VecDeque<Rect>,
    paint_all: bool,
    buffer_image: BufferImage,
}

impl Drop for WindowInner {
    fn drop(&mut self) {
        xcb::destroy_window(self.conn.conn(), self.window_id);
    }
}

struct X11GraphicsContext<'a> {
    buffer: &'a mut BitmapImage,
}

impl<'a> PaintContext for X11GraphicsContext<'a> {
    fn clear_rect(
        &mut self,
        dest_x: isize,
        dest_y: isize,
        width: usize,
        height: usize,
        color: Color,
    ) {
        self.buffer.clear_rect(dest_x, dest_y, width, height, color)
    }

    fn clear(&mut self, color: Color) {
        self.buffer.clear(color);
    }

    fn get_dimensions(&self) -> Dimensions {
        let (pixel_width, pixel_height) = self.buffer.image_dimensions();
        Dimensions {
            pixel_width,
            pixel_height,
            dpi: 96,
        }
    }

    fn draw_image_subset(
        &mut self,
        dest_x: isize,
        dest_y: isize,
        src_x: usize,
        src_y: usize,
        width: usize,
        height: usize,
        im: &dyn BitmapImage,
        operator: Operator,
    ) {
        self.buffer
            .draw_image_subset(dest_x, dest_y, src_x, src_y, width, height, im, operator)
    }
}

impl WindowInner {
    fn paint(&mut self) -> Fallible<()> {
        let window_dimensions = Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        };

        if self.paint_all {
            self.paint_all = false;
            self.expose.clear();
            self.expose.push_back(window_dimensions);
        } else if self.expose.is_empty() {
            return Ok(());
        }

        let (buf_width, buf_height) = self.buffer_image.image_dimensions();
        if buf_width != self.width.into() || buf_height != self.height.into() {
            // Window was resized, so we need to update our buffer
            self.buffer_image = BufferImage::new(
                &self.conn,
                self.window_id,
                self.width as usize,
                self.height as usize,
            );
        }

        for rect in self.expose.drain(..) {
            // Clip the rectangle to the current window size.
            // It can be larger than the window size in the case where we are working
            // through a series of resize exposures during a live resize, and we're
            // now sized smaller then when we queued the exposure.
            let rect = Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width.min(self.width),
                height: rect.height.min(self.height),
            };

            eprintln!("paint {:?}", rect);

            let mut context = X11GraphicsContext {
                buffer: &mut self.buffer_image,
            };

            self.callbacks.paint(&mut context);

            match &self.buffer_image {
                BufferImage::Shared(ref im) => {
                    self.window_context.copy_area(
                        im,
                        rect.x as i16,
                        rect.y as i16,
                        &self.window_id,
                        rect.x as i16,
                        rect.y as i16,
                        rect.width,
                        rect.height,
                    );
                }
                BufferImage::Image(ref buffer) => {
                    if rect == window_dimensions {
                        self.window_context.put_image(0, 0, buffer);
                    } else {
                        let mut im = Image::new(rect.width as usize, rect.height as usize);

                        im.draw_image_subset(
                            0,
                            0,
                            rect.x as usize,
                            rect.y as usize,
                            rect.width as usize,
                            rect.height as usize,
                            buffer,
                            Operator::Source,
                        );

                        self.window_context
                            .put_image(rect.x as i16, rect.y as i16, &im);
                    }
                }
            }
        }

        Ok(())
    }

    /// Add a region to the list of exposed/damaged/dirty regions.
    /// Note that a window resize will likely invalidate the entire window.
    /// If the new region intersects with the prior region, then we expand
    /// it to encompass both.  This avoids bloating the list with a series
    /// of increasing rectangles when resizing larger or smaller.
    fn expose(&mut self, x: u16, y: u16, width: u16, height: u16) {
        let expose = Rect {
            x,
            y,
            width,
            height,
        };
        if let Some(prior) = self.expose.back_mut() {
            if prior.intersects_with(&expose) {
                *prior = prior.enclosing_boundary_with(&expose);
                return;
            }
        }
        self.expose.push_back(expose);
    }

    fn dispatch_event(&mut self, event: &xcb::GenericEvent) -> Fallible<()> {
        let r = event.response_type() & 0x7f;
        match r {
            xcb::EXPOSE => {
                let expose: &xcb::ExposeEvent = unsafe { xcb::cast_event(event) };
                self.expose(expose.x(), expose.y(), expose.width(), expose.height());
            }
            xcb::CONFIGURE_NOTIFY => {
                let cfg: &xcb::ConfigureNotifyEvent = unsafe { xcb::cast_event(event) };
                self.width = cfg.width();
                self.height = cfg.height();
                self.callbacks.resize(Dimensions {
                    pixel_width: self.width as usize,
                    pixel_height: self.height as usize,
                    dpi: 96,
                })
            }
            xcb::KEY_PRESS | xcb::KEY_RELEASE => {
                let key_press: &xcb::KeyPressEvent = unsafe { xcb::cast_event(event) };
                if let Some((code, mods)) = self.conn.keyboard.process_key_event(key_press) {
                    let key = KeyEvent {
                        key: code,
                        modifiers: mods,
                        repeat_count: 1,
                        key_is_down: r == xcb::KEY_PRESS,
                    };
                    self.callbacks.key_event(&key);
                }
            }

            xcb::MOTION_NOTIFY => {
                let motion: &xcb::MotionNotifyEvent = unsafe { xcb::cast_event(event) };

                let event = MouseEvent {
                    kind: MouseEventKind::Move,
                    x: motion.event_x().max(0) as u16,
                    y: motion.event_y().max(0) as u16,
                    modifiers: xkeysyms::modifiers_from_state(motion.state()),
                    mouse_buttons: MouseButtons::default(),
                };
                self.callbacks.mouse_event(&event);
            }
            xcb::BUTTON_PRESS | xcb::BUTTON_RELEASE => {
                let button_press: &xcb::ButtonPressEvent = unsafe { xcb::cast_event(event) };

                let kind = match button_press.detail() {
                    b @ 1..=3 => {
                        let button = match b {
                            1 => MousePress::Left,
                            2 => MousePress::Middle,
                            3 => MousePress::Right,
                            _ => unreachable!(),
                        };
                        if r == xcb::BUTTON_PRESS {
                            MouseEventKind::Press(button)
                        } else {
                            MouseEventKind::Release(button)
                        }
                    }
                    b @ 4..=5 => {
                        if r == xcb::BUTTON_RELEASE {
                            return Ok(());
                        }
                        MouseEventKind::VertWheel(if b == 4 { 1 } else { -1 })
                    }
                    _ => {
                        eprintln!("button {} is not implemented", button_press.detail());
                        return Ok(());
                    }
                };

                let event = MouseEvent {
                    kind,
                    x: button_press.event_x().max(0) as u16,
                    y: button_press.event_y().max(0) as u16,
                    modifiers: xkeysyms::modifiers_from_state(button_press.state()),
                    mouse_buttons: MouseButtons::default(),
                };

                self.callbacks.mouse_event(&event);
            }
            xcb::CLIENT_MESSAGE => {
                let msg: &xcb::ClientMessageEvent = unsafe { xcb::cast_event(event) };
                if msg.data().data32()[0] == self.conn.atom_delete() {
                    if self.callbacks.can_close() {
                        xcb::destroy_window(self.conn.conn(), self.window_id);
                    }
                }
            }
            xcb::DESTROY_NOTIFY => {
                self.callbacks.destroy();
                self.conn.windows.borrow_mut().remove(&self.window_id);
            }
            _ => {
                eprintln!("unhandled: {:x}", r);
            }
        }

        Ok(())
    }
}

/// A Window!
#[derive(Clone)]
pub struct Window {
    window: Arc<Mutex<WindowInner>>,
}

impl Window {
    /// Create a new window on the specified screen with the specified
    /// dimensions
    pub fn new_window(
        _class_name: &str,
        name: &str,
        width: usize,
        height: usize,
        callbacks: Box<WindowCallbacks>,
    ) -> Fallible<Window> {
        let conn = Connection::get().ok_or_else(|| {
            failure::err_msg(
                "new_window must be called on the gui thread after Connection::init has succeeded",
            )
        })?;

        let window_id;
        let window = {
            let setup = conn.conn().get_setup();
            let screen = setup
                .roots()
                .nth(conn.screen_num() as usize)
                .ok_or_else(|| failure::err_msg("no screen?"))?;

            window_id = conn.conn().generate_id();

            xcb::create_window_checked(
                conn.conn(),
                xcb::COPY_FROM_PARENT as u8,
                window_id,
                screen.root(),
                // x, y
                0,
                0,
                // width, height
                width.try_into()?,
                height.try_into()?,
                // border width
                0,
                xcb::WINDOW_CLASS_INPUT_OUTPUT as u16,
                screen.root_visual(),
                &[(
                    xcb::CW_EVENT_MASK,
                    xcb::EVENT_MASK_EXPOSURE
                        | xcb::EVENT_MASK_KEY_PRESS
                        | xcb::EVENT_MASK_BUTTON_PRESS
                        | xcb::EVENT_MASK_BUTTON_RELEASE
                        | xcb::EVENT_MASK_POINTER_MOTION
                        | xcb::EVENT_MASK_BUTTON_MOTION
                        | xcb::EVENT_MASK_KEY_RELEASE
                        | xcb::EVENT_MASK_STRUCTURE_NOTIFY,
                )],
            )
            .request_check()?;

            let window_context = Context::new(&conn, &window_id);

            let buffer_image = BufferImage::new(&conn, window_id, width, height);

            Arc::new(Mutex::new(WindowInner {
                window_id,
                conn: Arc::clone(&conn),
                callbacks: callbacks,
                window_context,
                width: width.try_into()?,
                height: height.try_into()?,
                expose: VecDeque::new(),
                paint_all: true,
                buffer_image,
            }))
        };

        xcb::change_property(
            &*conn,
            xcb::PROP_MODE_REPLACE as u8,
            window_id,
            conn.atom_protocols,
            4,
            32,
            &[conn.atom_delete],
        );

        let window = Window { window };

        conn.windows.borrow_mut().insert(window_id, window.clone());

        window.set_title(name);
        window.show();

        Ok(window)
    }

    /// Change the title for the window manager
    pub fn set_title(&self, title: &str) {
        let window = self.window.lock().unwrap();
        xcb_util::icccm::set_wm_name(window.conn.conn(), window.window_id, title);
    }

    /// Display the window
    pub fn show(&self) {
        let window = self.window.lock().unwrap();
        xcb::map_window(window.conn.conn(), window.window_id);
    }

    pub fn dispatch_event(&self, event: &xcb::GenericEvent) -> Fallible<()> {
        self.window.lock().unwrap().dispatch_event(event)
    }

    pub(crate) fn paint_if_needed(&self) -> Fallible<()> {
        self.window.lock().unwrap().paint()
    }
}

impl Drawable for Window {
    fn as_drawable(&self) -> xcb::xproto::Drawable {
        self.window.lock().unwrap().window_id
    }
}