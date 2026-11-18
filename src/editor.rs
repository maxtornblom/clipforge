//! The clip editor: live preview, trim timeline, export.

#![allow(deprecated)] // gtk::VolumeButton

use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{atomic::Ordering, Arc},
    sync::atomic::AtomicBool,
    time::Duration,
};

use adw::prelude::*;
use gst::prelude::*;
use gtk::{gdk, gdk_pixbuf::Pixbuf, gio, glib};

use crate::{
    config::Config,
    media::{self, BarInfo, ExportJob, Framing, EDITOR_POOL},
    timeline::Timeline,
};

const MIN_LEN: f64 = 0.1;
const STRIP_FRAMES: u32 = 16;

#[derive(Default)]
struct St {
    path: Option<PathBuf>,
    duration: f64,
    pos: f64,
    playing: bool,
    in_pt: f64,
    out_pt: f64,
    /// Bumped on every load so late async results for a previous clip are dropped.
    generation: u32,
}

pub struct Editor {
    pub page: adw::NavigationPage,
    cfg: Rc<RefCell<Config>>,
    toasts: adw::ToastOverlay,
    title: adw::WindowTitle,
    picture: gtk::Picture,
    aspect: gtk::AspectFrame,
    preview_badge: gtk::Label,
    playbin: Option<gst::Element>,
    bus_guard: RefCell<Option<gst::bus::BusWatchGuard>>,
    play_btn: gtk::Button,
    time_label: gtk::Label,
    timeline: Timeline,
    in_spin: gtk::SpinButton,
    out_spin: gtk::SpinButton,
    len_label: gtk::Label,
    exact_btn: gtk::ToggleButton,
    export_btn: gtk::Button,
    framing: gtk::DropDown,
    bars: RefCell<Option<BarInfo>>,
    frame: Cell<Option<(u32, u32)>>,
    progress_box: gtk::Box,
    progress: gtk::ProgressBar,
    notice: gtk::Label,
    st: RefCell<St>,
    tick: RefCell<Option<glib::SourceId>>,
    updating: Cell<bool>,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
    on_changed: RefCell<Option<Box<dyn Fn()>>>,
}

fn make_playbin() -> Result<(gst::Element, gdk::Paintable), String> {
    let sink = gst::ElementFactory::make("gtk4paintablesink")
        .build()
        .map_err(|_| "gtk4paintablesink not found (install gst-plugin-gtk4)".to_string())?;
    let paintable = sink.property::<gdk::Paintable>("paintable");
    let playbin = gst::ElementFactory::make("playbin")
        .build()
        .map_err(|e| e.to_string())?;
    playbin.set_property("video-sink", &sink);
    Ok((playbin, paintable))
}

impl Editor {
    pub fn new(cfg: Rc<RefCell<Config>>, toasts: adw::ToastOverlay) -> Rc<Self> {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_can_shrink(true);

        let (playbin, sink_error) = match make_playbin() {
            Ok((pb, paintable)) => {
                picture.set_paintable(Some(&paintable));
                (Some(pb), None)
            }
            Err(e) => (None, Some(e)),
        };

        let title = adw::WindowTitle::new("", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let video_frame = gtk::Overlay::new();
        video_frame.add_css_class("video-frame");
        // The aspect frame lets the preview show the clip stretched to 16:9
        // (Fill) exactly as an export would, or as recorded (Original).
        let aspect = gtk::AspectFrame::new(0.5, 0.5, 16.0 / 9.0, true);
        aspect.set_hexpand(true);
        aspect.set_vexpand(true);
        aspect.set_child(Some(&picture));
        video_frame.set_child(Some(&aspect));
        video_frame.set_vexpand(true);

        let preview_badge = gtk::Label::new(None);
        preview_badge.add_css_class("osd-badge");
        preview_badge.set_halign(gtk::Align::Start);
        preview_badge.set_valign(gtk::Align::End);
        preview_badge.set_margin_start(12);
        preview_badge.set_margin_bottom(12);
        preview_badge.set_visible(false);
        video_frame.add_overlay(&preview_badge);

        let notice = gtk::Label::new(None);
        notice.add_css_class("osd-badge");
        notice.set_halign(gtk::Align::Center);
        notice.set_valign(gtk::Align::Start);
        notice.set_margin_top(12);
        notice.set_visible(false);
        video_frame.add_overlay(&notice);

        // Transport row
        let play_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play_btn.add_css_class("circular");
        play_btn.add_css_class("suggested-action");
        play_btn.set_tooltip_text(Some("Play / Pause (Space)"));
        play_btn.set_sensitive(playbin.is_some());

        let time_label = gtk::Label::new(Some("0:00.0 / 0:00"));
        time_label.add_css_class("numeric");
        time_label.add_css_class("dim-label");

        let volume = gtk::VolumeButton::new();
        volume.set_value(1.0);

        let transport = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        transport.append(&play_btn);
        transport.append(&time_label);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        transport.append(&spacer);
        transport.append(&volume);

        let timeline = Timeline::new();

        // Trim row
        let in_spin = gtk::SpinButton::with_range(0.0, 1.0, 0.1);
        let out_spin = gtk::SpinButton::with_range(0.0, 1.0, 0.1);
        for s in [&in_spin, &out_spin] {
            s.set_digits(1);
            s.set_width_chars(7);
            s.set_numeric(true);
        }
        let set_in = gtk::Button::with_label("Set start");
        set_in.set_tooltip_text(Some("Trim start at playhead (I)"));
        let set_out = gtk::Button::with_label("Set end");
        set_out.set_tooltip_text(Some("Trim end at playhead (O)"));
        let len_label = gtk::Label::new(None);
        len_label.add_css_class("dim-label");
        len_label.add_css_class("numeric");

        let trim_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        for w in [
            labelled("Start", &in_spin),
            set_in.clone().upcast::<gtk::Widget>(),
            labelled("End", &out_spin),
            set_out.clone().upcast::<gtk::Widget>(),
            len_label.clone().upcast::<gtk::Widget>(),
        ] {
            trim_row.append(&w);
        }

        // Export row
        let fast_btn = gtk::ToggleButton::with_label("Fast");
        fast_btn.set_tooltip_text(Some("Copy without re-encoding: instant, cuts snap to the nearest keyframe"));
        let exact_btn = gtk::ToggleButton::with_label("Precise");
        exact_btn.set_tooltip_text(Some("Re-encode for frame-accurate cuts: slower"));
        exact_btn.set_group(Some(&fast_btn));
        if cfg.borrow().exact_export {
            exact_btn.set_active(true);
        } else {
            fast_btn.set_active(true);
        }
        let mode_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        mode_box.add_css_class("linked");
        mode_box.append(&fast_btn);
        mode_box.append(&exact_btn);

        let framing = gtk::DropDown::from_strings(&["Original framing"]);
        framing.set_sensitive(false);
        framing.set_tooltip_text(Some("Looking for black bars…"));

        let export_btn = gtk::Button::with_label("Export clip");
        export_btn.add_css_class("suggested-action");
        export_btn.add_css_class("pill");

        let progress = gtk::ProgressBar::new();
        progress.set_hexpand(true);
        progress.set_valign(gtk::Align::Center);
        progress.set_show_text(true);
        let cancel_btn = gtk::Button::with_label("Cancel");
        let progress_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        progress_box.append(&progress);
        progress_box.append(&cancel_btn);
        progress_box.set_hexpand(true);
        progress_box.set_visible(false);

        trim_row.append(&{
            let s = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            s.set_hexpand(true);
            s
        });
        trim_row.append(&framing);
        trim_row.append(&mode_box);
        trim_row.append(&export_btn);

        let controls = gtk::Box::new(gtk::Orientation::Vertical, 10);
        controls.add_css_class("editor-controls");
        controls.append(&transport);
        controls.append(&timeline.widget);
        controls.append(&trim_row);
        controls.append(&progress_box);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&video_frame);
        content.append(&controls);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&content));

        let page = adw::NavigationPage::new(&view, "Editor");
        page.set_tag(Some("editor"));

        let ed = Rc::new(Editor {
            page,
            cfg,
            toasts,
            title,
            picture,
            aspect,
            preview_badge,
            playbin,
            bus_guard: RefCell::new(None),
            play_btn,
            time_label,
            timeline,
            in_spin,
            out_spin,
            len_label,
            exact_btn,
            export_btn,
            framing,
            bars: RefCell::new(None),
            frame: Cell::new(None),
            progress_box,
            progress,
            notice,
            st: RefCell::default(),
            tick: RefCell::new(None),
            updating: Cell::new(false),
            cancel: RefCell::new(None),
            on_changed: RefCell::new(None),
        });

        if let Some(e) = sink_error {
            ed.notice.set_label("Live preview unavailable: scrub the timeline to see frames");
            ed.notice.set_visible(true);
            eprintln!("clipforge: {e}");
        }
        ed.setup_bus();

        // Wiring
        let w = Rc::downgrade(&ed);
        macro_rules! with {
            ($body:expr) => {{
                let w = w.clone();
                move |_: &_| {
                    if let Some(ed) = w.upgrade() {
                        #[allow(clippy::redundant_closure_call)]
                        ($body)(&ed);
                    }
                }
            }};
        }
        ed.play_btn.connect_clicked(with!(|ed: &Rc<Editor>| ed.toggle_play()));
        set_in.connect_clicked(with!(|ed: &Rc<Editor>| ed.set_in_here()));
        set_out.connect_clicked(with!(|ed: &Rc<Editor>| ed.set_out_here()));
        ed.export_btn.connect_clicked(with!(|ed: &Rc<Editor>| ed.ask_export()));
        cancel_btn.connect_clicked(with!(|ed: &Rc<Editor>| {
            if let Some(c) = ed.cancel.borrow().as_ref() {
                c.store(true, Ordering::Relaxed);
            }
        }));
        {
            let w = w.clone();
            click_toggle(&ed.picture, move || {
                if let Some(ed) = w.upgrade() {
                    ed.toggle_play();
                }
            });
        }
        {
            let w = w.clone();
            volume.connect_value_changed(move |_, v| {
                if let Some(ed) = w.upgrade() {
                    if let Some(pb) = &ed.playbin {
                        pb.set_property("volume", v * v); // perceptual curve
                    }
                }
            });
        }
        for spin in [&ed.in_spin, &ed.out_spin] {
            let w = w.clone();
            spin.connect_value_changed(move |_| {
                if let Some(ed) = w.upgrade() {
                    if !ed.updating.get() {
                        ed.apply_range(ed.in_spin.value(), ed.out_spin.value());
                    }
                }
            });
        }
        {
            let w = w.clone();
            ed.framing.connect_selected_notify(move |_| {
                if let Some(ed) = w.upgrade() {
                    ed.apply_preview();
                }
            });
        }
        {
            let w = w.clone();
            ed.exact_btn.connect_toggled(move |b| {
                if let Some(ed) = w.upgrade() {
                    ed.cfg.borrow_mut().exact_export = b.is_active();
                    ed.cfg.borrow().save();
                }
            });
        }
        {
            let w = w.clone();
            ed.timeline.connect_seek(move |t, final_| {
                if let Some(ed) = w.upgrade() {
                    ed.seek(t, final_);
                }
            });
        }
        {
            let w = w.clone();
            ed.timeline.connect_range(move |a, b| {
                if let Some(ed) = w.upgrade() {
                    ed.apply_range(a, b);
                }
            });
        }
        {
            let w = w.clone();
            ed.page.connect_hidden(move |_| {
                if let Some(ed) = w.upgrade() {
                    ed.unload();
                }
            });
        }
        ed
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Box::new(f));
    }

    fn setup_bus(self: &Rc<Self>) {
        let Some(pb) = &self.playbin else { return };
        let Some(bus) = pb.bus() else { return };
        let w = Rc::downgrade(self);
        let guard = bus.add_watch_local(move |_, msg| {
            let Some(ed) = w.upgrade() else {
                return glib::ControlFlow::Break;
            };
            use gst::MessageView as M;
            match msg.view() {
                M::StateChanged(s) => {
                    let from_playbin = ed
                        .playbin
                        .as_ref()
                        .is_some_and(|pb| msg.src().is_some_and(|o| o == pb.upcast_ref::<gst::Object>()));
                    if from_playbin {
                        let playing = s.current() == gst::State::Playing;
                        ed.st.borrow_mut().playing = playing;
                        ed.play_btn.set_icon_name(if playing {
                            "media-playback-pause-symbolic"
                        } else {
                            "media-playback-start-symbolic"
                        });
                    }
                }
                M::Eos(_) => {
                    let start = ed.st.borrow().in_pt;
                    ed.pause();
                    ed.seek(start, true);
                }
                M::Error(e) => {
                    eprintln!("clipforge: GStreamer error: {} ({:?})", e.error(), e.debug());
                    ed.toast(&format!("Playback error: {}", e.error()));
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        });
        *self.bus_guard.borrow_mut() = guard.ok();
    }

    // -- Loading ------------------------------------------------------------

    pub fn load(self: &Rc<Self>, path: &Path, known_duration: f64) {
        self.unload();
        let generation = {
            let mut st = self.st.borrow_mut();
            st.generation += 1;
            *st = St {
                path: Some(path.to_owned()),
                duration: known_duration,
                generation: st.generation,
                ..St::default()
            };
            st.generation
        };
        self.title.set_title(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        self.picture.set_filename(None::<&str>);
        self.timeline.set_strip(None, STRIP_FRAMES);
        self.timeline.set_playhead(0.0);
        self.progress_box.set_visible(false);
        self.export_btn.set_sensitive(true);
        *self.bars.borrow_mut() = None;
        self.frame.set(None);
        self.framing.set_model(Some(&gtk::StringList::new(&["Original framing"])));
        self.framing.set_selected(0);
        self.framing.set_sensitive(false);
        self.framing.set_tooltip_text(Some("Looking for black bars…"));

        if let Some(pb) = &self.playbin {
            pb.set_property("uri", gio::File::for_path(path).uri().as_str());
            let _ = pb.set_state(gst::State::Paused);
            let w = Rc::downgrade(self);
            let id = glib::timeout_add_local(Duration::from_millis(50), move || match w.upgrade() {
                Some(ed) => {
                    ed.on_tick();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
            *self.tick.borrow_mut() = Some(id);
        } else {
            self.show_scrub_frame(0.0);
        }

        if known_duration > 0.0 {
            self.init_duration(known_duration);
        } else {
            let p = path.to_owned();
            let ed = self.clone();
            glib::spawn_future_local(async move {
                let d = media::LIBRARY_POOL.run(move || media::probe_duration(&p)).await.unwrap_or(0.0);
                if d > 0.0 && ed.st.borrow().generation == generation && ed.st.borrow().duration <= 0.0 {
                    ed.init_duration(d);
                    ed.load_strip(generation);
                }
            });
        }
        if known_duration > 0.0 {
            self.load_strip(generation);
        }
    }

    fn load_strip(self: &Rc<Self>, generation: u32) {
        let (path, dur) = {
            let st = self.st.borrow();
            (st.path.clone(), st.duration)
        };
        let Some(path) = path else { return };
        {
            let (ed, path) = (self.clone(), path.clone());
            glib::spawn_future_local(async move {
                let (frame, bars) = EDITOR_POOL
                    .run(move || (media::frame_size(&path), media::detect_bars(&path, dur)))
                    .await
                    .unwrap_or((None, None));
                if ed.st.borrow().generation != generation {
                    return;
                }
                let Some(frame) = frame else {
                    ed.framing.set_tooltip_text(Some("Couldn't read the video size"));
                    return;
                };
                ed.frame.set(Some(frame));
                let can_fill = bars.is_some() || !media::is_16_9(frame);
                if !can_fill {
                    ed.framing.set_tooltip_text(Some("Already fullscreen 16:9, no black bars"));
                    return;
                }
                let (fw, fh) = media::fill_size(frame);
                let mut items = vec!["Original framing", "Fill screen (stretch to 16:9)"];
                let tip = match bars {
                    Some(b) => {
                        items.push("Crop to content (remove bars)");
                        format!(
                            "Black bars detected: the picture is {}×{} inside {}×{}. Fill stretches it to {fw}×{fh}; Crop keeps its shape.",
                            b.content.w, b.content.h, frame.0, frame.1
                        )
                    }
                    None => format!("This clip is {}×{}. Fill stretches it to {fw}×{fh} (16:9).", frame.0, frame.1),
                };
                *ed.bars.borrow_mut() = bars;
                ed.framing.set_model(Some(&gtk::StringList::new(&items)));
                ed.framing.set_tooltip_text(Some(&format!("{tip} Re-encodes the clip.")));
                ed.framing.set_sensitive(true);
                // Plain 4:3 gameplay opens already stretched, so the preview is fullscreen at once.
                let fill_default = ed.cfg.borrow().fill_43_default && bars.is_none() && media::is_four_three(frame);
                ed.framing.set_selected(if fill_default { 1 } else { 0 });
            });
        }
        let ed = self.clone();
        glib::spawn_future_local(async move {
            let strip = EDITOR_POOL
                .run(move || media::ensure_filmstrip(&path, dur, STRIP_FRAMES))
                .await
                .flatten();
            if ed.st.borrow().generation == generation {
                let pb = strip.and_then(|p| Pixbuf::from_file(p).ok());
                ed.timeline.set_strip(pb, STRIP_FRAMES);
            }
        });
    }

    fn init_duration(&self, d: f64) {
        self.st.borrow_mut().duration = d;
        self.timeline.set_duration(d);
        self.updating.set(true);
        self.in_spin.set_range(0.0, d);
        self.out_spin.set_range(0.0, d);
        self.updating.set(false);
        self.apply_range(0.0, d);
        self.update_position(0.0);
    }

    fn unload(&self) {
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
        if let Some(pb) = &self.playbin {
            let _ = pb.set_state(gst::State::Null);
        }
        let mut st = self.st.borrow_mut();
        st.playing = false;
        st.generation += 1;
        drop(st);
        self.play_btn.set_icon_name("media-playback-start-symbolic");
    }

    // -- Playback -----------------------------------------------------------

    fn on_tick(&self) {
        let Some(pb) = &self.playbin else { return };
        if self.st.borrow().duration <= 0.0 {
            if let Some(d) = pb.query_duration::<gst::ClockTime>() {
                let d = d.seconds_f64();
                if d > 0.0 {
                    self.init_duration(d);
                }
            }
        }
        if !self.st.borrow().playing {
            return;
        }
        let Some(pos) = pb.query_position::<gst::ClockTime>() else { return };
        let pos = pos.seconds_f64();
        let (in_pt, out_pt) = {
            let st = self.st.borrow();
            (st.in_pt, st.out_pt)
        };
        if pos >= out_pt - 0.02 && out_pt > in_pt {
            // Reached the end of the selection: stop and return to its start.
            self.pause();
            self.seek(in_pt, true);
        } else {
            self.update_position(pos);
        }
    }

    fn update_position(&self, pos: f64) {
        let dur = {
            let mut st = self.st.borrow_mut();
            st.pos = pos;
            st.duration
        };
        self.timeline.set_playhead(pos);
        self.time_label
            .set_label(&format!("{} / {}", media::precise_duration(pos), media::human_duration(dur)));
    }

    fn pause(&self) {
        if let Some(pb) = &self.playbin {
            let _ = pb.set_state(gst::State::Paused);
        }
    }

    pub fn toggle_play(&self) {
        let Some(pb) = &self.playbin else { return };
        let (playing, pos, in_pt, out_pt) = {
            let st = self.st.borrow();
            (st.playing, st.pos, st.in_pt, st.out_pt)
        };
        if playing {
            self.pause();
        } else {
            if pos < in_pt || pos >= out_pt - 0.05 {
                self.seek(in_pt, true);
            }
            let _ = pb.set_state(gst::State::Playing);
        }
    }

    pub fn seek(&self, t: f64, accurate: bool) {
        let dur = self.st.borrow().duration;
        let t = if dur > 0.0 { t.clamp(0.0, dur) } else { t.max(0.0) };
        self.update_position(t);
        if let Some(pb) = &self.playbin {
            let flags = gst::SeekFlags::FLUSH
                | if accurate { gst::SeekFlags::ACCURATE } else { gst::SeekFlags::KEY_UNIT };
            let _ = pb.seek_simple(flags, gst::ClockTime::from_nseconds((t * 1e9) as u64));
        } else if accurate {
            self.show_scrub_frame(t);
        }
    }

    fn show_scrub_frame(&self, t: f64) {
        let Some(path) = self.st.borrow().path.clone() else { return };
        let picture = self.picture.clone();
        glib::spawn_future_local(async move {
            if let Some(f) = EDITOR_POOL.run(move || media::scrub_frame(&path, t)).await.flatten() {
                picture.set_filename(Some(&f));
            }
        });
    }

    // -- Trim points --------------------------------------------------------

    fn apply_range(&self, a: f64, b: f64) {
        let dur = self.st.borrow().duration;
        if dur <= 0.0 {
            return;
        }
        let a = a.clamp(0.0, (dur - MIN_LEN).max(0.0));
        let b = b.clamp((a + MIN_LEN).min(dur), dur);
        {
            let mut st = self.st.borrow_mut();
            st.in_pt = a;
            st.out_pt = b;
        }
        self.timeline.set_range(a, b);
        self.updating.set(true);
        if (self.in_spin.value() - a).abs() > 1e-6 {
            self.in_spin.set_value(a);
        }
        if (self.out_spin.value() - b).abs() > 1e-6 {
            self.out_spin.set_value(b);
        }
        self.updating.set(false);
        self.len_label.set_label(&format!("Clip length {}", media::precise_duration(b - a)));
    }

    fn set_in_here(&self) {
        let (pos, out, dur) = {
            let st = self.st.borrow();
            (st.pos, st.out_pt, st.duration)
        };
        self.apply_range(pos, if out <= pos + MIN_LEN { dur } else { out });
    }

    fn set_out_here(&self) {
        let (pos, inp) = {
            let st = self.st.borrow();
            (st.pos, st.in_pt)
        };
        self.apply_range(if inp >= pos - MIN_LEN { 0.0 } else { inp }, pos);
    }

    // -- Keyboard -----------------------------------------------------------

    pub fn handle_key(&self, key: gdk::Key, state: gdk::ModifierType) -> bool {
        let step = if state.contains(gdk::ModifierType::SHIFT_MASK) { 1.0 } else { 5.0 };
        let pos = self.st.borrow().pos;
        match key {
            gdk::Key::space | gdk::Key::k => self.toggle_play(),
            gdk::Key::i | gdk::Key::I => self.set_in_here(),
            gdk::Key::o | gdk::Key::O => self.set_out_here(),
            gdk::Key::Left | gdk::Key::j => self.seek(pos - step, true),
            gdk::Key::Right | gdk::Key::l => self.seek(pos + step, true),
            gdk::Key::Home => self.seek(self.st.borrow().in_pt, true),
            gdk::Key::End => self.seek(self.st.borrow().out_pt - 0.5, true),
            gdk::Key::comma => self.seek(pos - 1.0 / 30.0, true),
            gdk::Key::period => self.seek(pos + 1.0 / 30.0, true),
            _ => return false,
        }
        true
    }

    // -- Export -------------------------------------------------------------

    /// Show the chosen framing in the video area right away.
    fn apply_preview(&self) {
        let framing = self.selected_framing();
        let has_bars = self.bars.borrow().is_some();
        match framing {
            // Bar removal needs a crop the video widget can't preview; show the
            // original and say so rather than a misleading picture.
            Framing::Fill | Framing::Crop if has_bars => {
                self.aspect.set_obey_child(true);
                self.picture.set_content_fit(gtk::ContentFit::Contain);
                self.preview_badge.set_label("Preview shows the original; bars are removed on export");
                self.preview_badge.set_visible(true);
            }
            Framing::Fill => {
                self.aspect.set_obey_child(false);
                self.aspect.set_ratio(16.0 / 9.0);
                self.picture.set_content_fit(gtk::ContentFit::Fill);
                self.preview_badge.set_label("Fullscreen 16:9 preview");
                self.preview_badge.set_visible(true);
            }
            _ => {
                self.aspect.set_obey_child(true);
                self.picture.set_content_fit(gtk::ContentFit::Contain);
                self.preview_badge.set_visible(false);
            }
        }
    }

    fn selected_framing(&self) -> Framing {
        if self.frame.get().is_none() {
            return Framing::Original;
        }
        match self.framing.selected() {
            1 => Framing::Fill,
            2 => Framing::Crop,
            _ => Framing::Original,
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn ask_export(self: &Rc<Self>) {
        if self.cancel.borrow().is_some() {
            return;
        }
        let (in_pt, out_pt) = {
            let st = self.st.borrow();
            (st.in_pt, st.out_pt)
        };
        if out_pt - in_pt < MIN_LEN {
            self.toast("The end point must be after the start point");
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Export clip"),
            Some(&format!(
                "{} – {}  ({} long){}",
                media::precise_duration(in_pt),
                media::precise_duration(out_pt),
                media::precise_duration(out_pt - in_pt),
                match self.selected_framing() {
                    Framing::Original => "",
                    Framing::Fill => "\nStretched to fill a 16:9 screen (re-encodes)",
                    Framing::Crop => "\nBlack bars cropped away (re-encodes)",
                }
            )),
        );
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            ("replace", "Replace Original"),
            ("copy", "Save as New Clip"),
        ]);
        dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("copy"));
        dialog.set_close_response("cancel");

        let ed = self.clone();
        glib::spawn_future_local(async move {
            match dialog.choose_future(Some(&ed.page)).await.as_str() {
                "copy" => ed.start_export(false),
                "replace" => ed.confirm_replace().await,
                _ => {}
            }
        });
    }

    async fn confirm_replace(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Replace the original clip?"),
            Some("The original file will be overwritten with the trimmed version. This cannot be undone."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("replace", "Replace")]);
        dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        if dialog.choose_future(Some(&self.page)).await == "replace" {
            self.start_export(true);
        }
    }

    fn start_export(self: &Rc<Self>, replace: bool) {
        let (src, start, end) = {
            let st = self.st.borrow();
            (st.path.clone(), st.in_pt, st.out_pt)
        };
        let Some(src) = src else { return };
        let dst = if replace { src.clone() } else { media::unique_clip_path(&src) };
        let tmp = media::temp_path_for(&dst);
        let job = ExportJob {
            src: src.clone(),
            start,
            duration: end - start,
            exact: self.exact_btn.is_active(),
            framing: self.selected_framing(),
            bars: *self.bars.borrow(),
            frame: self.frame.get().unwrap_or((0, 0)),
        };

        self.pause();
        if replace {
            if let Some(pb) = &self.playbin {
                let _ = pb.set_state(gst::State::Null);
            }
        }
        let cancel = media::cancel_flag();
        *self.cancel.borrow_mut() = Some(cancel.clone());
        self.export_btn.set_sensitive(false);
        self.progress.set_fraction(0.0);
        self.progress.set_text(Some("Exporting…"));
        self.progress_box.set_visible(true);

        enum Msg {
            Progress(f64),
            Done(Result<(), String>),
        }
        let (tx, rx) = async_channel::unbounded::<Msg>();
        {
            let (tmp, dst) = (tmp.clone(), dst.clone());
            std::thread::spawn(move || {
                let txp = tx.clone();
                let result = media::run_export(&job, &tmp, &cancel, move |p| {
                    let _ = txp.send_blocking(Msg::Progress(p));
                })
                .and_then(|_| fs::rename(&tmp, &dst).map_err(|e| format!("Could not save clip: {e}")));
                if result.is_err() {
                    let _ = fs::remove_file(&tmp);
                }
                let _ = tx.send_blocking(Msg::Done(result));
            });
        }

        let ed = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Msg::Progress(p) => {
                        ed.progress.set_fraction(p);
                        ed.progress.set_text(Some(&format!("Exporting… {:.0}%", p * 100.0)));
                    }
                    Msg::Done(result) => {
                        *ed.cancel.borrow_mut() = None;
                        ed.progress_box.set_visible(false);
                        ed.export_btn.set_sensitive(true);
                        match result {
                            Ok(()) => {
                                let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                let toast = adw::Toast::new(&if replace {
                                    format!("Replaced {name}")
                                } else {
                                    format!("Saved {name}")
                                });
                                toast.set_button_label(Some("Show"));
                                toast.set_action_name(Some("win.clip-folder"));
                                toast.set_action_target_value(Some(&dst.to_string_lossy().to_variant()));
                                ed.toasts.add_toast(toast);
                                if let Some(f) = ed.on_changed.borrow().as_ref() {
                                    f();
                                }
                                if replace {
                                    let dur = media::probe_duration(&dst);
                                    ed.load(&src, dur);
                                }
                            }
                            Err(e) if e == "Cancelled" => {
                                ed.toast("Export cancelled");
                                if replace {
                                    ed.load(&src, ed.st.borrow().duration);
                                }
                            }
                            Err(e) => {
                                ed.toast(&format!("Export failed: {e}"));
                                if replace {
                                    ed.load(&src, ed.st.borrow().duration);
                                }
                            }
                        }
                        break;
                    }
                }
            }
        });
    }
}

fn labelled(text: &str, w: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let l = gtk::Label::new(Some(text));
    l.add_css_class("dim-label");
    b.append(&l);
    b.append(w);
    b.upcast()
}

fn click_toggle(w: &impl IsA<gtk::Widget>, f: impl Fn() + 'static) {
    let g = gtk::GestureClick::new();
    g.connect_released(move |_, n, _, _| {
        if n == 1 {
            f();
        }
    });
    w.add_controller(g);
}
