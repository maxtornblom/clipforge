//! Medal-style trim timeline: filmstrip, draggable in/out handles, playhead.

use std::{cell::RefCell, rc::Rc};

use gtk::{gdk::prelude::GdkCairoContextExt, gdk_pixbuf::Pixbuf, prelude::*};

const HANDLE_W: f64 = 14.0;
const GRAB: f64 = 16.0;
const MIN_LEN: f64 = 0.1;
const ACCENT: (f64, f64, f64) = (0.96, 0.77, 0.09);

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    In,
    Out,
    Playhead,
}

struct State {
    duration: f64,
    playhead: f64,
    in_pt: f64,
    out_pt: f64,
    frames: Vec<Pixbuf>,
    drag: Drag,
}

type SeekCb = Box<dyn Fn(f64, bool)>;
type RangeCb = Box<dyn Fn(f64, f64)>;

#[derive(Clone)]
pub struct Timeline {
    pub widget: gtk::DrawingArea,
    st: Rc<RefCell<State>>,
    on_seek: Rc<RefCell<Option<SeekCb>>>,
    on_range: Rc<RefCell<Option<RangeCb>>>,
}

impl Timeline {
    pub fn new() -> Self {
        let widget = gtk::DrawingArea::new();
        widget.set_content_height(76);
        widget.set_hexpand(true);
        widget.add_css_class("timeline");

        let st = Rc::new(RefCell::new(State {
            duration: 1.0,
            playhead: 0.0,
            in_pt: 0.0,
            out_pt: 1.0,
            frames: Vec::new(),
            drag: Drag::None,
        }));
        let tl = Timeline {
            widget,
            st,
            on_seek: Rc::default(),
            on_range: Rc::default(),
        };

        {
            let st = tl.st.clone();
            tl.widget.set_draw_func(move |_, cr, w, h| draw(&st.borrow(), cr, w as f64, h as f64));
        }

        let drag = gtk::GestureDrag::new();
        {
            let tl = tl.clone();
            drag.connect_drag_begin(move |_, x, _| {
                let (target, t) = {
                    let st = tl.st.borrow();
                    let w = tl.widget.width() as f64;
                    let in_x = time_to_x(&st, st.in_pt, w);
                    let out_x = time_to_x(&st, st.out_pt, w);
                    // Handles sit outside the selection, so grab zones are
                    // biased outward; when they overlap, nearest wins.
                    let d_in = (x - (in_x - HANDLE_W / 2.0)).abs();
                    let d_out = (x - (out_x + HANDLE_W / 2.0)).abs();
                    let target = if d_in < GRAB && d_in <= d_out {
                        Drag::In
                    } else if d_out < GRAB {
                        Drag::Out
                    } else {
                        Drag::Playhead
                    };
                    (target, x_to_time(&st, x, w))
                };
                tl.st.borrow_mut().drag = target;
                if target == Drag::Playhead {
                    tl.set_playhead(t);
                    tl.emit_seek(t, false);
                }
            });
        }
        {
            let tl = tl.clone();
            drag.connect_drag_update(move |g, dx, _| {
                let Some((sx, _)) = g.start_point() else { return };
                let (target, t) = {
                    let st = tl.st.borrow();
                    (st.drag, x_to_time(&st, sx + dx, tl.widget.width() as f64))
                };
                match target {
                    Drag::In => {
                        let out = tl.st.borrow().out_pt;
                        tl.set_range(t.min(out - MIN_LEN), out);
                        tl.emit_range();
                    }
                    Drag::Out => {
                        let inp = tl.st.borrow().in_pt;
                        tl.set_range(inp, t.max(inp + MIN_LEN));
                        tl.emit_range();
                    }
                    Drag::Playhead => {
                        tl.set_playhead(t);
                        tl.emit_seek(t, false);
                    }
                    Drag::None => {}
                }
            });
        }
        {
            let tl = tl.clone();
            drag.connect_drag_end(move |_, _, _| {
                let (target, t) = {
                    let st = tl.st.borrow();
                    (st.drag, st.playhead)
                };
                tl.st.borrow_mut().drag = Drag::None;
                if target == Drag::Playhead {
                    tl.emit_seek(t, true);
                }
            });
        }
        tl.widget.add_controller(drag);

        // Resize cursor over the handles.
        let motion = gtk::EventControllerMotion::new();
        {
            let tl = tl.clone();
            motion.connect_motion(move |_, x, _| {
                let st = tl.st.borrow();
                let w = tl.widget.width() as f64;
                let near_in = (x - (time_to_x(&st, st.in_pt, w) - HANDLE_W / 2.0)).abs() < GRAB;
                let near_out = (x - (time_to_x(&st, st.out_pt, w) + HANDLE_W / 2.0)).abs() < GRAB;
                tl.widget.set_cursor_from_name(Some(if near_in || near_out { "ew-resize" } else { "pointer" }));
            });
        }
        tl.widget.add_controller(motion);

        tl
    }

    pub fn connect_seek(&self, f: impl Fn(f64, bool) + 'static) {
        *self.on_seek.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_range(&self, f: impl Fn(f64, f64) + 'static) {
        *self.on_range.borrow_mut() = Some(Box::new(f));
    }

    fn emit_seek(&self, t: f64, final_: bool) {
        if let Some(cb) = self.on_seek.borrow().as_ref() {
            cb(t, final_);
        }
    }

    fn emit_range(&self) {
        let (a, b) = self.range();
        if let Some(cb) = self.on_range.borrow().as_ref() {
            cb(a, b);
        }
    }

    pub fn range(&self) -> (f64, f64) {
        let st = self.st.borrow();
        (st.in_pt, st.out_pt)
    }

    pub fn set_duration(&self, d: f64) {
        self.st.borrow_mut().duration = d.max(MIN_LEN);
        self.widget.queue_draw();
    }

    pub fn set_playhead(&self, t: f64) {
        {
            let mut st = self.st.borrow_mut();
            st.playhead = t.clamp(0.0, st.duration);
        }
        self.widget.queue_draw();
    }

    pub fn set_range(&self, a: f64, b: f64) {
        {
            let mut st = self.st.borrow_mut();
            let d = st.duration;
            st.in_pt = a.clamp(0.0, d);
            st.out_pt = b.clamp(0.0, d);
        }
        self.widget.queue_draw();
    }

    /// Slice a horizontal tile image into `count` frames.
    pub fn set_strip(&self, strip: Option<Pixbuf>, count: u32) {
        let mut frames = Vec::new();
        if let Some(pb) = strip {
            let fw = pb.width() / count as i32;
            if fw > 0 {
                for i in 0..count as i32 {
                    frames.push(pb.new_subpixbuf(i * fw, 0, fw, pb.height()));
                }
            }
        }
        self.st.borrow_mut().frames = frames;
        self.widget.queue_draw();
    }
}

fn usable(w: f64) -> f64 {
    (w - 2.0 * HANDLE_W).max(1.0)
}

fn time_to_x(st: &State, t: f64, w: f64) -> f64 {
    HANDLE_W + (t / st.duration).clamp(0.0, 1.0) * usable(w)
}

fn x_to_time(st: &State, x: f64, w: f64) -> f64 {
    (((x - HANDLE_W) / usable(w)) * st.duration).clamp(0.0, st.duration)
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

fn draw(st: &State, cr: &gtk::cairo::Context, w: f64, h: f64) {
    let (ar, ag, ab) = ACCENT;
    let bar_y = 8.0;
    let bar_h = h - 2.0 * bar_y;

    // Filmstrip, clipped to a rounded track.
    let _ = cr.save();
    rounded_rect(cr, HANDLE_W, bar_y, usable(w), bar_h, 6.0);
    cr.clip();
    cr.set_source_rgb(0.09, 0.09, 0.11);
    let _ = cr.paint();
    if !st.frames.is_empty() {
        let n = st.frames.len() as f64;
        let fw = usable(w) / n;
        for (i, pb) in st.frames.iter().enumerate() {
            let (pw, ph) = (pb.width() as f64, pb.height() as f64);
            let scale = (bar_h / ph).max(fw / pw);
            let x = HANDLE_W + i as f64 * fw;
            let _ = cr.save();
            cr.rectangle(x, bar_y, fw + 1.0, bar_h);
            cr.clip();
            cr.translate(x + (fw - pw * scale) / 2.0, bar_y + (bar_h - ph * scale) / 2.0);
            cr.scale(scale, scale);
            cr.set_source_pixbuf(pb, 0.0, 0.0);
            let _ = cr.paint();
            let _ = cr.restore();
        }
    }

    let in_x = time_to_x(st, st.in_pt, w);
    let out_x = time_to_x(st, st.out_pt, w);

    // Dim everything outside the selection.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.62);
    cr.rectangle(HANDLE_W, bar_y, in_x - HANDLE_W, bar_h);
    cr.rectangle(out_x, bar_y, w - HANDLE_W - out_x, bar_h);
    let _ = cr.fill();
    let _ = cr.restore();

    // Selection frame: top and bottom rails.
    cr.set_source_rgb(ar, ag, ab);
    cr.rectangle(in_x, bar_y - 3.0, out_x - in_x, 3.0);
    cr.rectangle(in_x, bar_y + bar_h, out_x - in_x, 3.0);
    let _ = cr.fill();

    // Handles.
    for (x, left) in [(in_x - HANDLE_W, true), (out_x, false)] {
        cr.set_source_rgb(ar, ag, ab);
        rounded_rect(cr, x, bar_y - 3.0, HANDLE_W, bar_h + 6.0, 5.0);
        let _ = cr.fill();
        // Grip
        cr.set_source_rgba(0.1, 0.08, 0.0, 0.75);
        let gx = x + HANDLE_W / 2.0 + if left { 0.5 } else { -0.5 };
        for dy in [-5.0, 0.0, 5.0] {
            cr.arc(gx, h / 2.0 + dy, 1.4, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    }

    // Playhead.
    let px = time_to_x(st, st.playhead, w);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.4);
    cr.rectangle(px - 2.0, 2.0, 4.0, h - 4.0);
    let _ = cr.fill();
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.rectangle(px - 1.0, 2.0, 2.0, h - 4.0);
    let _ = cr.fill();
    rounded_rect(cr, px - 5.0, 0.0, 10.0, 8.0, 3.0);
    let _ = cr.fill();
}
