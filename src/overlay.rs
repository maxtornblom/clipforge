//! A brief gold border around the screen that confirms a clip was saved.
//! Drawn through the Wayland layer-shell protocol so it sits above fullscreen
//! games, never takes focus and never intercepts clicks.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::{gdk, gio, glib, prelude::*};
use layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const HOLD_MS: f64 = 120.0;
const FADE_MS: f64 = 520.0;

struct Surface {
    connector: Option<String>,
    window: gtk::Window,
}

pub struct Flash {
    surfaces: RefCell<Vec<Surface>>,
    generation: Cell<u32>,
}

fn draw_border(cr: &gtk::cairo::Context, w: f64, h: f64) {
    // A bright outer line with a soft inward glow.
    for i in 0..7 {
        let t = i as f64 / 7.0;
        cr.set_source_rgba(0.96, 0.77, 0.09, 0.95 * (1.0 - t).powi(2));
        cr.set_line_width(4.0);
        let inset = 2.0 + i as f64 * 3.5;
        cr.rectangle(inset, inset, w - 2.0 * inset, h - 2.0 * inset);
        let _ = cr.stroke();
    }
}

fn build_window(monitor: &gdk::Monitor) -> gtk::Window {
    let win = gtk::Window::new();
    win.init_layer_shell();
    win.set_layer(Layer::Overlay);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        win.set_anchor(edge, true);
    }
    win.set_exclusive_zone(-1);
    win.set_keyboard_mode(KeyboardMode::None);
    win.set_namespace(Some("clipforge-flash"));
    win.set_monitor(Some(monitor));
    win.set_decorated(false);
    win.set_focusable(false);
    win.add_css_class("flash-window");

    let area = gtk::DrawingArea::new();
    area.set_draw_func(|_, cr, w, h| draw_border(cr, w as f64, h as f64));
    win.set_child(Some(&area));

    // An empty input region makes the whole window click-through.
    win.connect_realize(|w| {
        if let Some(surface) = w.surface() {
            surface.set_input_region(Some(&gtk::cairo::Region::create()));
        }
    });
    win
}

impl Flash {
    /// None if the compositor can't do layer-shell (the border is then simply skipped).
    pub fn new() -> Option<Rc<Self>> {
        if !layer_shell::is_supported() {
            eprintln!("clipforge: compositor lacks layer-shell; screen border disabled");
            return None;
        }
        let display = gdk::Display::default()?;
        let monitors = display.monitors();
        let surfaces = (0..monitors.n_items())
            .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
            .map(|m| Surface {
                connector: m.connector().map(|c| c.to_string()),
                window: build_window(&m),
            })
            .collect();
        Some(Rc::new(Flash { surfaces: RefCell::new(surfaces), generation: Cell::new(0) }))
    }

    /// Flash the monitor called `connector` (e.g. "DP-1"), or every monitor if it
    /// isn't known.
    pub fn pulse(self: &Rc<Self>, connector: Option<&str>) {
        let surfaces = self.surfaces.borrow();
        let any_match = connector.is_some_and(|c| surfaces.iter().any(|s| s.connector.as_deref() == Some(c)));
        let targets: Vec<gtk::Window> = surfaces
            .iter()
            .filter(|s| !any_match || s.connector.as_deref() == connector)
            .map(|s| s.window.clone())
            .collect();
        drop(surfaces);

        if std::env::var_os("CLIPFORGE_DEBUG").is_some() {
            eprintln!("clipforge: flash on {} window(s), connector {connector:?}", targets.len());
        }
        let gen = self.generation.get() + 1;
        self.generation.set(gen);
        for win in &targets {
            win.set_opacity(1.0);
            win.set_visible(true);
        }

        // Hold briefly, then fade out on the frame clock.
        let me = self.clone();
        let start = Rc::new(Cell::new(None::<i64>));
        let first = targets.first().cloned();
        let Some(first) = first else { return };
        first.add_tick_callback(move |_, clock| {
            if me.generation.get() != gen {
                return glib::ControlFlow::Break; // a newer flash took over
            }
            let now = clock.frame_time();
            let t0 = start.get().unwrap_or_else(|| {
                start.set(Some(now));
                now
            });
            let ms = (now - t0) as f64 / 1000.0;
            let opacity = if ms <= HOLD_MS { 1.0 } else { (1.0 - (ms - HOLD_MS) / FADE_MS).max(0.0).powi(2) };
            for w in &targets {
                w.set_opacity(opacity);
                if opacity <= 0.0 {
                    w.set_visible(false);
                }
            }
            if opacity <= 0.0 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
        });
    }
}

/// Tell a running ClipForge to flash (used by `clipforge --save`). Best effort and
/// quick: if ClipForge isn't running there is simply nothing to show.
pub fn request_flash() {
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
    let params = ("flash", Vec::<glib::Variant>::new(), std::collections::HashMap::<String, glib::Variant>::new()).to_variant();
    let _ = conn.call_sync(
        Some("se.local.clipforge"),
        "/se/local/clipforge",
        "org.gtk.Actions",
        "Activate",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        300,
        gio::Cancellable::NONE,
    );
}
