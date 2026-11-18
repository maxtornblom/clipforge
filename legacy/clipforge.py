#!/usr/bin/env python3
"""
ClipForge — a simple "Medal-like" clip manager for GPU Screen Recorder.

Features:
- Browse all clips from your GSR folder, with thumbnails
- Preview clips (live video if GStreamer/GTK4 sink is available, otherwise
  a scrubbable filmstrip of extracted frames — works everywhere)
- Medal-style timeline: drag the start/end handles to select the part you
  want to keep, then export as a new file or overwrite the original
- Rename / delete clips, open containing folder

Built with plain GTK4 (no libadwaita) — fits KDE Plasma / Breeze well.

Dependencies (Arch):
    sudo pacman -S python-gobject gtk4 gstreamer gst-plugins-base \
                    gst-plugins-good gst-plugins-bad gst-libav ffmpeg

Run:
    python3 clipforge.py
"""

import os
import sys
import json
import subprocess
import threading
import traceback
import hashlib
from datetime import datetime
from pathlib import Path

import gi
gi.require_version("Gtk", "4.0")

from gi.repository import Gtk, GLib, Gdk, GObject, Gio, Pango, GdkPixbuf

APP_ID = "se.local.clipforge"
CONFIG_DIR = Path(GLib.get_user_config_dir()) / "clipforge"
CONFIG_FILE = CONFIG_DIR / "config.json"
CACHE_DIR = Path(GLib.get_user_cache_dir()) / "clipforge" / "thumbs"
FILMSTRIP_DIR = Path(GLib.get_user_cache_dir()) / "clipforge" / "filmstrips"
VIDEO_EXTS = {".mp4", ".mkv", ".webm", ".mov"}

DEFAULT_CLIPS_DIR = str(Path.home() / "Videos")

# Try to init GStreamer + the GTK4 paintable sink. If anything fails, we fall
# back to a filmstrip-based scrubber (frames extracted with ffmpeg).
GST_OK = False
GST_ERROR = ""
try:
    gi.require_version("Gst", "1.0")
    gi.require_version("GstVideo", "1.0")
    from gi.repository import Gst, GstVideo
    Gst.init(None)
    _test_sink = Gst.ElementFactory.make("gtk4paintablesink", "test_sink")
    if _test_sink is None:
        GST_ERROR = "gtk4paintablesink not found (install gst-plugins-good / gst-plugins-bad, GTK4 build)"
    else:
        GST_OK = True
except Exception as e:
    GST_ERROR = str(e)


# ---------------------------------------------------------------------------
# Config helpers
# ---------------------------------------------------------------------------

def load_config():
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    if CONFIG_FILE.exists():
        try:
            return json.loads(CONFIG_FILE.read_text())
        except Exception:
            pass
    cfg = {"clips_dir": os.environ.get("GSR_CLIPS_DIR", DEFAULT_CLIPS_DIR)}
    save_config(cfg)
    return cfg


def save_config(cfg):
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    CONFIG_FILE.write_text(json.dumps(cfg, indent=2))


def human_size(num_bytes):
    for unit in ["B", "KB", "MB", "GB"]:
        if num_bytes < 1024:
            return f"{num_bytes:.0f} {unit}"
        num_bytes /= 1024
    return f"{num_bytes:.1f} TB"


def human_duration(seconds):
    seconds = max(0, int(seconds))
    h, rem = divmod(seconds, 3600)
    m, s = divmod(rem, 60)
    if h:
        return f"{h}:{m:02d}:{s:02d}"
    return f"{m}:{s:02d}"


def probe_duration(path):
    try:
        out = subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration",
             "-of", "default=noprint_wrappers=1:nokey=1", str(path)],
            capture_output=True, text=True, timeout=10
        )
        return float(out.stdout.strip())
    except Exception:
        return 0.0


def file_key(path: Path):
    try:
        stat = path.stat()
        return f"{path.name}-{stat.st_mtime_ns}-{stat.st_size}"
    except OSError:
        return path.name


def ensure_thumbnail(path: Path):
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    h = hashlib.sha1(file_key(path).encode()).hexdigest()
    out = CACHE_DIR / f"{h}.jpg"
    if out.exists():
        return out
    try:
        subprocess.run(
            ["ffmpeg", "-y", "-ss", "1", "-i", str(path),
             "-frames:v", "1", "-vf", "scale=320:-1",
             "-q:v", "4", str(out)],
            capture_output=True, timeout=20
        )
        if out.exists():
            return out
    except Exception:
        pass
    return None


def ensure_filmstrip(path: Path, duration: float, count: int = 12):
    """Extract `count` evenly-spaced frames for a scrub filmstrip.
    Returns a list of frame file paths (may be shorter than `count` on error)."""
    if duration <= 0:
        return []
    h = hashlib.sha1(file_key(path).encode()).hexdigest()
    out_dir = FILMSTRIP_DIR / h
    out_dir.mkdir(parents=True, exist_ok=True)

    existing = sorted(out_dir.glob("frame_*.jpg"))
    if len(existing) == count:
        return existing

    # Clear stale frames
    for f in existing:
        f.unlink(missing_ok=True)

    frames = []
    step = duration / count
    for i in range(count):
        ts = max(0.0, i * step)
        out = out_dir / f"frame_{i:03d}.jpg"
        try:
            subprocess.run(
                ["ffmpeg", "-y", "-ss", f"{ts:.3f}", "-i", str(path),
                 "-frames:v", "1", "-vf", "scale=160:-1",
                 "-q:v", "5", str(out)],
                capture_output=True, timeout=15
            )
            if out.exists():
                frames.append(out)
        except Exception:
            pass
    return frames


# ---------------------------------------------------------------------------
# Data model for a clip
# ---------------------------------------------------------------------------

class ClipItem(GObject.Object):
    __gtype_name__ = "ClipItem"

    path = GObject.Property(type=str)
    name = GObject.Property(type=str)
    size = GObject.Property(type=str)
    date = GObject.Property(type=str)
    duration = GObject.Property(type=str)
    thumb_path = GObject.Property(type=str)
    mtime = GObject.Property(type=float)

    def __init__(self, path: Path):
        super().__init__()
        st = path.stat()
        self.path = str(path)
        self.name = path.name
        self.size = human_size(st.st_size)
        self.mtime = st.st_mtime
        self.date = datetime.fromtimestamp(st.st_mtime).strftime("%Y-%m-%d %H:%M")
        self.duration = ""
        self.thumb_path = ""


# ---------------------------------------------------------------------------
# Live video preview widget (GStreamer + GTK paintable)
# ---------------------------------------------------------------------------

class GstVideoPlayer(Gtk.Box):
    """Live video preview using playbin + gtk4paintablesink."""

    __gsignals__ = {
        "position-updated": (GObject.SignalFlags.RUN_FIRST, None, (float, float)),
    }

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self._duration = 0.0
        self._tick_id = None

        self.picture = Gtk.Picture()
        self.picture.set_vexpand(True)
        self.picture.set_hexpand(True)
        self.picture.set_content_fit(Gtk.ContentFit.CONTAIN)
        frame = Gtk.Frame()
        frame.set_child(self.picture)
        frame.add_css_class("video-frame")
        self.append(frame)

        self.playbin = Gst.ElementFactory.make("playbin", "player")
        self.sink = Gst.ElementFactory.make("gtk4paintablesink", "gtk_sink")
        if self.playbin is None or self.sink is None:
            raise RuntimeError("Could not create playbin or gtk4paintablesink")
        self.playbin.set_property("video-sink", self.sink)
        paintable = self.sink.get_property("paintable")
        if paintable is None:
            raise RuntimeError("gtk4paintablesink returned no paintable")
        self.picture.set_paintable(paintable)

        self.bus = self.playbin.get_bus()
        self.bus.add_signal_watch()
        self.bus.connect("message::eos", self._on_eos)
        self.bus.connect("message::error", self._on_error)

    def load(self, path: str):
        self.stop()
        uri = Gst.filename_to_uri(path)
        self.playbin.set_property("uri", uri)
        self.playbin.set_state(Gst.State.PAUSED)
        self._duration = 0.0

    def play(self):
        self.playbin.set_state(Gst.State.PLAYING)
        if self._tick_id is None:
            self._tick_id = GLib.timeout_add(150, self._tick)

    def pause(self):
        self.playbin.set_state(Gst.State.PAUSED)

    def stop(self):
        self.playbin.set_state(Gst.State.NULL)
        if self._tick_id is not None:
            GLib.source_remove(self._tick_id)
            self._tick_id = None

    def is_playing(self):
        _, state, _ = self.playbin.get_state(0)
        return state == Gst.State.PLAYING

    def toggle_play(self):
        if self.is_playing():
            self.pause()
        else:
            self.play()

    def get_duration(self):
        if self._duration <= 0:
            ok, dur = self.playbin.query_duration(Gst.Format.TIME)
            if ok:
                self._duration = dur / Gst.SECOND
        return self._duration

    def get_position(self):
        ok, pos = self.playbin.query_position(Gst.Format.TIME)
        return pos / Gst.SECOND if ok else 0.0

    def seek(self, seconds):
        seconds = max(0.0, seconds)
        self.playbin.seek_simple(
            Gst.Format.TIME,
            Gst.SeekFlags.FLUSH | Gst.SeekFlags.KEY_UNIT,
            int(seconds * Gst.SECOND)
        )

    def _tick(self):
        dur = self.get_duration()
        pos = self.get_position()
        self.emit("position-updated", pos, dur)
        return True

    def _on_eos(self, bus, msg):
        self.pause()
        self.seek(0)

    def _on_error(self, bus, msg):
        err, debug = msg.parse_error()
        print("GStreamer error:", err, debug)


# ---------------------------------------------------------------------------
# Filmstrip-based preview (fallback when GStreamer/GTK sink unavailable)
# ---------------------------------------------------------------------------

class FilmstripPlayer(Gtk.Box):
    """Shows a single still frame, updated as the user scrubs the timeline.
    No real playback — but lets you see what's at any given timestamp,
    which is enough for trimming."""

    __gsignals__ = {
        "position-updated": (GObject.SignalFlags.RUN_FIRST, None, (float, float)),
    }

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.picture = Gtk.Picture()
        self.picture.set_vexpand(True)
        self.picture.set_hexpand(True)
        self.picture.set_content_fit(Gtk.ContentFit.CONTAIN)
        frame = Gtk.Frame()
        frame.set_child(self.picture)
        frame.add_css_class("video-frame")
        self.append(frame)

        hint = Gtk.Label(
            label="Live preview unavailable — showing frame at current position.\n"
                  "Drag the timeline below to preview different points."
        )
        hint.add_css_class("dim-label")
        hint.add_css_class("caption")
        hint.set_justify(Gtk.Justification.CENTER)
        hint.set_margin_top(4)
        hint.set_margin_bottom(4)
        self.append(hint)

        self.clip_path = None
        self.duration = 0.0
        self.position = 0.0
        self._frame_cache = {}

    def load(self, path: str):
        self.clip_path = path
        self.duration = probe_duration(path)
        self.position = 0.0
        self._frame_cache = {}
        self.seek(0)

    def get_duration(self):
        return self.duration

    def get_position(self):
        return self.position

    def seek(self, seconds):
        seconds = max(0.0, min(seconds, self.duration if self.duration else seconds))
        self.position = seconds
        self.emit("position-updated", self.position, self.duration)
        self._show_frame(seconds)

    def _show_frame(self, ts):
        key = round(ts, 1)
        if key in self._frame_cache:
            self.picture.set_filename(str(self._frame_cache[key]))
            return

        def work():
            tmp = CACHE_DIR / f"scrub_{hashlib.sha1(self.clip_path.encode()).hexdigest()}_{key}.jpg"
            try:
                subprocess.run(
                    ["ffmpeg", "-y", "-ss", f"{ts:.3f}", "-i", self.clip_path,
                     "-frames:v", "1", "-vf", "scale=960:-1",
                     "-q:v", "3", str(tmp)],
                    capture_output=True, timeout=10
                )
                if tmp.exists():
                    self._frame_cache[key] = tmp

                    def done():
                        if abs(self.position - ts) < 0.05:
                            self.picture.set_filename(str(tmp))
                        return False
                    GLib.idle_add(done)
            except Exception:
                pass

        threading.Thread(target=work, daemon=True).start()

    # Stubs to match GstVideoPlayer interface
    def play(self):
        pass

    def pause(self):
        pass

    def stop(self):
        pass

    def is_playing(self):
        return False

    def toggle_play(self):
        pass


def make_player():
    if GST_OK:
        try:
            return GstVideoPlayer(), True
        except Exception as e:
            print("GstVideoPlayer init failed, falling back:", e)
    return FilmstripPlayer(), False


# ---------------------------------------------------------------------------
# Medal-style timeline scrubber with draggable in/out handles
# ---------------------------------------------------------------------------

class TimelineWidget(Gtk.DrawingArea):
    """A horizontal timeline showing filmstrip thumbnails, a playhead, and
    two draggable handles marking the selected (trim) range.

    Implemented with Gtk.DrawingArea + Cairo for maximum compatibility
    (avoids GSK/Graphene snapshot API quirks)."""

    __gsignals__ = {
        "playhead-changed": (GObject.SignalFlags.RUN_FIRST, None, (float,)),
        "range-changed": (GObject.SignalFlags.RUN_FIRST, None, (float, float)),
    }

    HANDLE_WIDTH = 10
    HEIGHT = 64

    def __init__(self):
        super().__init__()
        self.set_size_request(-1, self.HEIGHT)
        self.set_hexpand(True)
        self.set_vexpand(False)
        self.duration = 1.0
        self.playhead = 0.0
        self.in_point = 0.0
        self.out_point = 1.0
        self._drag_target = None  # "in", "out", "playhead", or None

        self.frame_pixbufs = []  # list of GdkPixbuf.Pixbuf

        self.set_draw_func(self._on_draw, None)

        drag = Gtk.GestureDrag()
        drag.connect("drag-begin", self._on_drag_begin)
        drag.connect("drag-update", self._on_drag_update)
        self.add_controller(drag)

        click = Gtk.GestureClick()
        click.connect("pressed", self._on_click)
        self.add_controller(click)

    def set_duration(self, duration):
        self.duration = max(duration, 0.1)
        self.queue_draw()

    def set_frames(self, frame_paths):
        pixbufs = []
        for p in frame_paths:
            try:
                pixbufs.append(GdkPixbuf.Pixbuf.new_from_file(str(p)))
            except Exception:
                pass
        self.frame_pixbufs = pixbufs
        self.queue_draw()

    def set_playhead(self, t):
        self.playhead = max(0.0, min(t, self.duration))
        self.queue_draw()

    def set_range(self, in_point, out_point):
        self.in_point = max(0.0, min(in_point, self.duration))
        self.out_point = max(0.0, min(out_point, self.duration))
        self.queue_draw()

    def _x_to_time(self, x):
        w = self.get_width() or 1
        usable = max(w - 2 * self.HANDLE_WIDTH, 1)
        frac = (x - self.HANDLE_WIDTH) / usable
        return max(0.0, min(frac * self.duration, self.duration))

    def _time_to_x(self, t):
        w = self.get_width() or 1
        usable = max(w - 2 * self.HANDLE_WIDTH, 1)
        frac = t / self.duration if self.duration else 0
        return self.HANDLE_WIDTH + frac * usable

    def _on_click(self, gesture, n_press, x, y):
        in_x = self._time_to_x(self.in_point)
        out_x = self._time_to_x(self.out_point)
        if abs(x - in_x) < 12 or abs(x - out_x) < 12:
            return  # let drag handle it
        t = self._x_to_time(x)
        self.set_playhead(t)
        self.emit("playhead-changed", t)

    def _on_drag_begin(self, gesture, start_x, start_y):
        in_x = self._time_to_x(self.in_point)
        out_x = self._time_to_x(self.out_point)
        if abs(start_x - in_x) < 12:
            self._drag_target = "in"
        elif abs(start_x - out_x) < 12:
            self._drag_target = "out"
        else:
            self._drag_target = "playhead"
            t = self._x_to_time(start_x)
            self.set_playhead(t)
            self.emit("playhead-changed", t)

    def _on_drag_update(self, gesture, offset_x, offset_y):
        ok, start_x, start_y = gesture.get_start_point()
        x = start_x + offset_x
        t = self._x_to_time(x)
        if self._drag_target == "in":
            self.in_point = max(0.0, min(t, self.out_point - 0.1))
            self.queue_draw()
            self.emit("range-changed", self.in_point, self.out_point)
        elif self._drag_target == "out":
            self.out_point = min(self.duration, max(t, self.in_point + 0.1))
            self.queue_draw()
            self.emit("range-changed", self.in_point, self.out_point)
        elif self._drag_target == "playhead":
            self.set_playhead(t)
            self.emit("playhead-changed", t)

    def _on_draw(self, area, cr, w, h, data):
        # Background
        cr.set_source_rgb(0.10, 0.10, 0.10)
        cr.rectangle(0, 0, w, h)
        cr.fill()

        # Filmstrip
        if self.frame_pixbufs:
            n = len(self.frame_pixbufs)
            usable = max(w - 2 * self.HANDLE_WIDTH, 1)
            fw = usable / n
            for i, pixbuf in enumerate(self.frame_pixbufs):
                x = self.HANDLE_WIDTH + i * fw
                pw, ph = pixbuf.get_width(), pixbuf.get_height()
                if ph <= 0:
                    continue
                scale = h / ph
                draw_w = pw * scale
                offset_x = x - (draw_w - fw) / 2

                cr.save()
                cr.rectangle(x, 0, fw + 1, h)
                cr.clip()
                cr.translate(offset_x, 0)
                cr.scale(scale, scale)
                Gdk.cairo_set_source_pixbuf(cr, pixbuf, 0, 0)
                cr.paint()
                cr.restore()

        in_x = self._time_to_x(self.in_point)
        out_x = self._time_to_x(self.out_point)

        # Dim areas outside selection
        cr.set_source_rgba(0, 0, 0, 0.55)
        if in_x > self.HANDLE_WIDTH:
            cr.rectangle(0, 0, in_x, h)
            cr.fill()
        if out_x < w - self.HANDLE_WIDTH:
            cr.rectangle(out_x, 0, w - out_x, h)
            cr.fill()

        # Selection border
        cr.set_source_rgb(0x35 / 255, 0x84 / 255, 0xe4 / 255)
        cr.rectangle(in_x - 2, 0, 3, h)
        cr.fill()
        cr.rectangle(out_x - 1, 0, 3, h)
        cr.fill()

        # Handles
        cr.rectangle(in_x - self.HANDLE_WIDTH, 0, self.HANDLE_WIDTH, h)
        cr.fill()
        cr.rectangle(out_x, 0, self.HANDLE_WIDTH, h)
        cr.fill()

        # Playhead
        ph_x = self._time_to_x(self.playhead)
        cr.set_source_rgb(1, 1, 1)
        cr.rectangle(ph_x - 1, 0, 2, h)
        cr.fill()
        cr.rectangle(ph_x - 4, 0, 8, 6)
        cr.fill()


# ---------------------------------------------------------------------------
# Editor view (Medal-style trim / export)
# ---------------------------------------------------------------------------

class EditorView(Gtk.Box):
    def __init__(self, window):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        self.window = window
        self.clip_path = None
        self.duration = 0.0
        self.set_margin_top(12)
        self.set_margin_bottom(12)
        self.set_margin_start(12)
        self.set_margin_end(12)

        # Header
        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        back_btn = Gtk.Button(icon_name="go-previous-symbolic")
        back_btn.set_tooltip_text("Back to library")
        back_btn.connect("clicked", lambda b: self.window.show_library())
        header.append(back_btn)

        self.title_label = Gtk.Label(label="Editor")
        self.title_label.add_css_class("title-2")
        self.title_label.set_hexpand(True)
        self.title_label.set_xalign(0)
        self.title_label.set_ellipsize(Pango.EllipsizeMode.MIDDLE)
        header.append(self.title_label)
        self.append(header)

        # Player (Gst or filmstrip fallback)
        self.player, self.player_is_gst = make_player()
        self.player.set_vexpand(True)
        self.player.connect("position-updated", self._on_position_updated)
        self.append(self.player)

        if not GST_OK:
            self.window.notify("Live video preview unavailable: " + GST_ERROR, sticky=True)

        # Transport controls
        transport = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        transport.set_halign(Gtk.Align.CENTER)

        self.play_btn = Gtk.Button(icon_name="media-playback-start-symbolic")
        self.play_btn.set_tooltip_text("Play/Pause (Space)")
        self.play_btn.connect("clicked", lambda b: self._toggle_play())
        self.play_btn.set_sensitive(self.player_is_gst)
        transport.append(self.play_btn)

        self.time_label = Gtk.Label(label="0:00 / 0:00")
        self.time_label.add_css_class("dim-label")
        self.time_label.set_width_chars(12)
        transport.append(self.time_label)
        self.append(transport)

        # Medal-style timeline
        self.timeline = TimelineWidget()
        self.timeline.connect("playhead-changed", self._on_playhead_changed)
        self.timeline.connect("range-changed", self._on_range_changed)
        self.append(self.timeline)

        # In/out info + jump buttons
        marks = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        marks.set_halign(Gtk.Align.CENTER)

        self.in_label = Gtk.Label(label="Start: 0:00")
        in_set = Gtk.Button(label="Set start (I)")
        in_set.connect("clicked", lambda b: self._set_in())
        in_goto = Gtk.Button(icon_name="go-first-symbolic")
        in_goto.set_tooltip_text("Jump to start point")
        in_goto.connect("clicked", lambda b: self._goto_in())

        self.out_label = Gtk.Label(label="End: 0:00")
        out_set = Gtk.Button(label="Set end (O)")
        out_set.connect("clicked", lambda b: self._set_out())
        out_goto = Gtk.Button(icon_name="go-last-symbolic")
        out_goto.set_tooltip_text("Jump to end point")
        out_goto.connect("clicked", lambda b: self._goto_out())

        for w in (in_goto, in_set, self.in_label, Gtk.Separator(), self.out_label, out_set, out_goto):
            marks.append(w)
        self.append(marks)

        self.in_point = 0.0
        self.out_point = 0.0

        # Manual time entry
        manual_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        manual_box.set_halign(Gtk.Align.CENTER)
        manual_box.append(Gtk.Label(label="Or set exact times (seconds):"))
        self.in_entry = Gtk.SpinButton.new_with_range(0, 999999, 0.1)
        self.in_entry.set_digits(1)
        manual_box.append(Gtk.Label(label="Start"))
        manual_box.append(self.in_entry)
        self.out_entry = Gtk.SpinButton.new_with_range(0, 999999, 0.1)
        self.out_entry.set_digits(1)
        manual_box.append(Gtk.Label(label="End"))
        manual_box.append(self.out_entry)
        apply_btn = Gtk.Button(label="Apply")
        apply_btn.connect("clicked", lambda b: self._apply_manual())
        manual_box.append(apply_btn)
        self.append(manual_box)

        # Export options
        export_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        export_box.set_margin_top(6)

        self.mode_combo = Gtk.DropDown.new_from_strings(
            ["Fast (no re-encode, may be a couple seconds off)", "Exact (re-encodes, slower)"]
        )
        self.mode_combo.set_hexpand(True)
        export_box.append(self.mode_combo)

        export_btn = Gtk.Button(label="Export clip")
        export_btn.add_css_class("suggested-action")
        export_btn.connect("clicked", lambda b: self._export())
        export_box.append(export_btn)
        self.append(export_box)

        self.status_label = Gtk.Label(label="")
        self.status_label.add_css_class("dim-label")
        self.status_label.set_xalign(0)
        self.append(self.status_label)

        # Keyboard shortcuts
        key_controller = Gtk.EventControllerKey()
        key_controller.connect("key-pressed", self._on_key_pressed)
        self.add_controller(key_controller)

    def load_clip(self, path: str):
        self.clip_path = path
        self.title_label.set_label(Path(path).name)
        self.status_label.set_label("")
        self.player.load(path)
        self.duration = probe_duration(path)
        self.in_point = 0.0
        self.out_point = self.duration
        self.in_label.set_label(f"Start: {human_duration(self.in_point)}")
        self.out_label.set_label(f"End: {human_duration(self.out_point)}")
        self.in_entry.set_range(0, max(self.duration, 0.1))
        self.out_entry.set_range(0, max(self.duration, 0.1))
        self.in_entry.set_value(self.in_point)
        self.out_entry.set_value(self.out_point)
        self.play_btn.set_icon_name("media-playback-start-symbolic")

        self.timeline.set_duration(self.duration)
        self.timeline.set_range(self.in_point, self.out_point)
        self.timeline.set_playhead(0)
        self.timeline.set_frames([])

        # Load filmstrip thumbnails async
        def work():
            frames = ensure_filmstrip(Path(path), self.duration, count=14)

            def done():
                self.timeline.set_frames(frames)
                return False
            GLib.idle_add(done)
        threading.Thread(target=work, daemon=True).start()

    def _apply_manual(self):
        self.in_point = self.in_entry.get_value()
        self.out_point = self.out_entry.get_value()
        self.in_label.set_label(f"Start: {human_duration(self.in_point)}")
        self.out_label.set_label(f"End: {human_duration(self.out_point)}")
        self.timeline.set_range(self.in_point, self.out_point)

    def _toggle_play(self):
        if not self.player_is_gst:
            return
        self.player.toggle_play()
        icon = "media-playback-pause-symbolic" if self.player.is_playing() else "media-playback-start-symbolic"
        self.play_btn.set_icon_name(icon)

    def _on_position_updated(self, player, pos, dur):
        if dur > 0 and abs(self.duration - dur) > 0.5:
            self.duration = dur
            self.timeline.set_duration(dur)
            self.in_entry.set_range(0, dur)
            self.out_entry.set_range(0, dur)
            if self.out_point == 0 or self.out_point > dur:
                self.out_point = dur
                self.out_label.set_label(f"End: {human_duration(self.out_point)}")
                self.out_entry.set_value(self.out_point)
                self.timeline.set_range(self.in_point, self.out_point)
        self.timeline.set_playhead(pos)
        self.time_label.set_label(f"{human_duration(pos)} / {human_duration(dur)}")

    def _on_playhead_changed(self, widget, t):
        self.player.seek(t)
        self.time_label.set_label(f"{human_duration(t)} / {human_duration(self.duration)}")

    def _on_range_changed(self, widget, in_point, out_point):
        self.in_point = in_point
        self.out_point = out_point
        self.in_label.set_label(f"Start: {human_duration(self.in_point)}")
        self.out_label.set_label(f"End: {human_duration(self.out_point)}")
        self.in_entry.set_value(self.in_point)
        self.out_entry.set_value(self.out_point)

    def _set_in(self):
        self.in_point = self.player.get_position()
        if self.in_point > self.out_point:
            self.out_point = self.duration
        self.in_label.set_label(f"Start: {human_duration(self.in_point)}")
        self.out_label.set_label(f"End: {human_duration(self.out_point)}")
        self.in_entry.set_value(self.in_point)
        self.out_entry.set_value(self.out_point)
        self.timeline.set_range(self.in_point, self.out_point)

    def _set_out(self):
        self.out_point = self.player.get_position()
        if self.out_point < self.in_point:
            self.in_point = 0
        self.in_label.set_label(f"Start: {human_duration(self.in_point)}")
        self.out_label.set_label(f"End: {human_duration(self.out_point)}")
        self.in_entry.set_value(self.in_point)
        self.out_entry.set_value(self.out_point)
        self.timeline.set_range(self.in_point, self.out_point)

    def _goto_in(self):
        self.player.seek(self.in_point)
        self.timeline.set_playhead(self.in_point)

    def _goto_out(self):
        self.player.seek(max(0, self.out_point - 0.5))
        self.timeline.set_playhead(max(0, self.out_point - 0.5))

    def _on_key_pressed(self, controller, keyval, keycode, state):
        name = Gdk.keyval_name(keyval)
        if name == "space":
            self._toggle_play()
            return True
        if name in ("i", "I"):
            self._set_in()
            return True
        if name in ("o", "O"):
            self._set_out()
            return True
        if name == "Left":
            t = max(0, self.player.get_position() - 5)
            self.player.seek(t)
            self.timeline.set_playhead(t)
            return True
        if name == "Right":
            t = self.player.get_position() + 5
            self.player.seek(t)
            self.timeline.set_playhead(t)
            return True
        return False

    # -- Export --

    def _export(self):
        if not self.clip_path:
            return
        if self.out_point <= self.in_point:
            self.status_label.set_label("Error: end point must be after start point.")
            return

        # Ask: Save as new clip, or overwrite the original?
        dialog = Gtk.Dialog(transient_for=self.window, modal=True, title="Save trimmed clip")
        dialog.set_default_size(420, -1)
        content = dialog.get_content_area()
        content.set_margin_top(16)
        content.set_margin_bottom(16)
        content.set_margin_start(16)
        content.set_margin_end(16)
        content.set_spacing(8)

        label = Gtk.Label(label="How do you want to save this clip?")
        label.set_xalign(0)
        content.append(label)

        sub = Gtk.Label(label=f"Selected range: {human_duration(self.in_point)} – {human_duration(self.out_point)} "
                               f"({human_duration(self.out_point - self.in_point)})")
        sub.add_css_class("dim-label")
        sub.set_xalign(0)
        content.append(sub)

        dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
        new_btn = dialog.add_button("Save as new clip…", Gtk.ResponseType.ACCEPT)
        new_btn.add_css_class("suggested-action")
        overwrite_btn = dialog.add_button("Overwrite original", Gtk.ResponseType.APPLY)
        overwrite_btn.add_css_class("destructive-action")

        def on_response(d, response):
            d.destroy()
            if response == Gtk.ResponseType.ACCEPT:
                self._export_as_new()
            elif response == Gtk.ResponseType.APPLY:
                self._export_overwrite()

        dialog.connect("response", on_response)
        dialog.present()

    def _export_as_new(self):
        src = Path(self.clip_path)
        suggested = f"{src.stem}_clip{src.suffix}"

        dialog = Gtk.FileDialog()
        dialog.set_title("Save clip as")
        dialog.set_initial_name(suggested)
        try:
            dialog.set_initial_folder(Gio.File.new_for_path(str(src.parent)))
        except Exception:
            pass

        def on_response(dlg, result):
            try:
                gfile = dlg.save_finish(result)
            except GLib.Error:
                return
            if gfile is None:
                return
            self._run_export(gfile.get_path(), overwrite_original=False)

        dialog.save(self.window, None, on_response)

    def _export_overwrite(self):
        # Confirm before destroying the original
        dialog = Gtk.MessageDialog(
            transient_for=self.window,
            modal=True,
            message_type=Gtk.MessageType.WARNING,
            buttons=Gtk.ButtonsType.NONE,
            text="Overwrite original clip?",
            secondary_text="This will replace the original file with the trimmed version. This cannot be undone.",
        )
        dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
        ok_btn = dialog.add_button("Overwrite", Gtk.ResponseType.OK)
        ok_btn.add_css_class("destructive-action")

        def on_response(d, response):
            d.destroy()
            if response == Gtk.ResponseType.OK:
                self._run_export(self.clip_path, overwrite_original=True)

        dialog.connect("response", on_response)
        dialog.present()

    def _run_export(self, out_path, overwrite_original):
        if self.player_is_gst:
            self.player.pause()
        start = self.in_point
        duration = self.out_point - self.in_point
        exact = self.mode_combo.get_selected() == 1

        self.status_label.set_label("Exporting...")

        def work():
            src = self.clip_path
            # If overwriting, export to a temp file first, then replace.
            target = out_path
            tmp_target = None
            if overwrite_original:
                tmp_target = str(Path(out_path).with_suffix(".clipforge_tmp" + Path(out_path).suffix))
                target = tmp_target

            cmd = ["ffmpeg", "-y"]
            if not exact:
                cmd += ["-ss", f"{start:.3f}", "-i", src,
                        "-t", f"{duration:.3f}", "-c", "copy", target]
            else:
                cmd += ["-i", src, "-ss", f"{start:.3f}",
                        "-t", f"{duration:.3f}",
                        "-c:v", "libx264", "-preset", "veryfast", "-crf", "18",
                        "-c:a", "aac", "-b:a", "192k", target]
            try:
                res = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
                ok = res.returncode == 0 and Path(target).exists()
            except Exception:
                ok = False
                res = None

            if ok and overwrite_original and tmp_target:
                try:
                    os.replace(tmp_target, out_path)
                except OSError:
                    ok = False

            def done():
                if ok:
                    self.status_label.set_label(f"Done! Saved to {out_path}")
                    self.window.refresh_library()
                    if overwrite_original:
                        # Reload the (now trimmed) clip
                        self.load_clip(out_path)
                else:
                    msg = res.stderr[-400:] if res else "unknown error"
                    self.status_label.set_label(f"Export failed: {msg}")
                    if tmp_target:
                        try:
                            Path(tmp_target).unlink(missing_ok=True)
                        except Exception:
                            pass
                return False

            GLib.idle_add(done)

        threading.Thread(target=work, daemon=True).start()


# ---------------------------------------------------------------------------
# Library view
# ---------------------------------------------------------------------------

class ClipCard(Gtk.Box):
    def __init__(self, item: ClipItem, on_open, on_delete, on_rename, on_folder):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        self.item = item
        self.add_css_class("card")
        self.set_size_request(220, 170)

        overlay = Gtk.Overlay()
        self.picture = Gtk.Picture()
        self.picture.set_size_request(220, 124)
        self.picture.set_content_fit(Gtk.ContentFit.COVER)
        self._set_thumb(item)
        overlay.set_child(self.picture)

        dur_label = Gtk.Label(label=item.duration or "")
        dur_label.add_css_class("osd-badge")
        dur_label.set_margin_end(6)
        dur_label.set_margin_bottom(6)
        dur_label.set_halign(Gtk.Align.END)
        dur_label.set_valign(Gtk.Align.END)
        overlay.add_overlay(dur_label)
        self._dur_label = dur_label

        self.append(overlay)

        name_label = Gtk.Label(label=item.name)
        name_label.set_ellipsize(Pango.EllipsizeMode.MIDDLE)
        name_label.set_xalign(0)
        name_label.set_margin_start(6)
        name_label.set_margin_end(6)
        self.append(name_label)

        meta_label = Gtk.Label(label=f"{item.date} · {item.size}")
        meta_label.add_css_class("caption")
        meta_label.add_css_class("dim-label")
        meta_label.set_xalign(0)
        meta_label.set_margin_start(6)
        meta_label.set_margin_end(6)
        meta_label.set_margin_bottom(4)
        self.append(meta_label)

        click = Gtk.GestureClick()
        click.connect("released", lambda *a: on_open(item))
        self.add_controller(click)

        # Right-click menu
        menu = Gio.Menu()
        menu.append("Edit", "card.edit")
        menu.append("Rename", "card.rename")
        menu.append("Show in file manager", "card.folder")
        menu.append("Delete", "card.delete")

        action_group = Gio.SimpleActionGroup()
        edit_action = Gio.SimpleAction.new("edit", None)
        edit_action.connect("activate", lambda a, p: on_open(item))
        rename_action = Gio.SimpleAction.new("rename", None)
        rename_action.connect("activate", lambda a, p: on_rename(item))
        delete_action = Gio.SimpleAction.new("delete", None)
        delete_action.connect("activate", lambda a, p: on_delete(item))
        folder_action = Gio.SimpleAction.new("folder", None)
        folder_action.connect("activate", lambda a, p: on_folder(item))
        for act in (edit_action, rename_action, delete_action, folder_action):
            action_group.add_action(act)
        self.insert_action_group("card", action_group)

        popover = Gtk.PopoverMenu.new_from_model(menu)
        popover.set_parent(self)
        self.popover = popover

        right_click = Gtk.GestureClick(button=3)
        right_click.connect("released", self._on_right_click)
        self.add_controller(right_click)

    def _on_right_click(self, gesture, n_press, x, y):
        rect = Gdk.Rectangle()
        rect.x, rect.y, rect.width, rect.height = int(x), int(y), 1, 1
        self.popover.set_pointing_to(rect)
        self.popover.popup()

    def _set_thumb(self, item):
        if item.thumb_path and Path(item.thumb_path).exists():
            self.picture.set_filename(item.thumb_path)
        else:
            self.picture.set_filename(None)

    def update_duration(self, text):
        self._dur_label.set_label(text)
        self.item.duration = text

    def update_thumb(self, thumb_path):
        self.item.thumb_path = thumb_path
        self._set_thumb(self.item)


class LibraryView(Gtk.Box):
    def __init__(self, window):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.window = window
        self.set_margin_top(12)
        self.set_margin_bottom(12)
        self.set_margin_start(12)
        self.set_margin_end(12)

        # Toolbar
        toolbar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self.search = Gtk.SearchEntry()
        self.search.set_placeholder_text("Search clips...")
        self.search.set_hexpand(True)
        self.search.connect("search-changed", lambda e: self._refilter())
        toolbar.append(self.search)

        sort_combo = Gtk.DropDown.new_from_strings(["Newest first", "Oldest first", "Name", "Size"])
        sort_combo.connect("notify::selected", lambda c, p: self._refilter())
        self.sort_combo = sort_combo
        toolbar.append(sort_combo)

        refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic")
        refresh_btn.set_tooltip_text("Refresh")
        refresh_btn.connect("clicked", lambda b: self.window.refresh_library())
        toolbar.append(refresh_btn)

        self.append(toolbar)

        self.scroller = Gtk.ScrolledWindow()
        self.scroller.set_vexpand(True)
        self.flowbox = Gtk.FlowBox()
        self.flowbox.set_valign(Gtk.Align.START)
        self.flowbox.set_max_children_per_line(8)
        self.flowbox.set_min_children_per_line(2)
        self.flowbox.set_selection_mode(Gtk.SelectionMode.NONE)
        self.flowbox.set_homogeneous(True)
        self.flowbox.set_row_spacing(8)
        self.flowbox.set_column_spacing(8)
        self.scroller.set_child(self.flowbox)
        self.append(self.scroller)

        self.empty_label = Gtk.Label(
            label="No clips found.\nCheck that the clips folder is set correctly via the menu (☰)."
        )
        self.empty_label.set_justify(Gtk.Justification.CENTER)
        self.empty_label.add_css_class("dim-label")
        self.empty_label.set_vexpand(True)
        self.empty_label.set_valign(Gtk.Align.CENTER)

        self.all_items = []

    def set_items(self, items):
        self.all_items = items
        self._refilter()

    def _refilter(self):
        query = self.search.get_text().lower().strip()
        items = [i for i in self.all_items if query in i.name.lower()]

        sort_idx = self.sort_combo.get_selected()
        if sort_idx == 0:
            items.sort(key=lambda i: i.mtime, reverse=True)
        elif sort_idx == 1:
            items.sort(key=lambda i: i.mtime)
        elif sort_idx == 2:
            items.sort(key=lambda i: i.name.lower())
        elif sort_idx == 3:
            items.sort(key=lambda i: Path(i.path).stat().st_size if Path(i.path).exists() else 0, reverse=True)

        for child in list(self.flowbox):
            self.flowbox.remove(child)

        if not items:
            if self.scroller.get_child() is not self.empty_label:
                self.scroller.set_child(self.empty_label)
            return
        if self.scroller.get_child() is not self.flowbox:
            self.scroller.set_child(self.flowbox)

        for item in items:
            card = ClipCard(
                item,
                on_open=self.window.open_editor,
                on_delete=self.window.delete_clip,
                on_rename=self.window.rename_clip,
                on_folder=self.window.show_in_folder,
            )
            self.flowbox.append(card)
            self._load_meta_async(item, card)

    def _load_meta_async(self, item, card):
        def work():
            dur = probe_duration(item.path)
            thumb = ensure_thumbnail(Path(item.path))

            def done():
                if dur > 0:
                    card.update_duration(human_duration(dur))
                if thumb:
                    card.update_thumb(str(thumb))
                return False
            GLib.idle_add(done)
        threading.Thread(target=work, daemon=True).start()


# ---------------------------------------------------------------------------
# Main window
# ---------------------------------------------------------------------------

class ClipForgeWindow(Gtk.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="ClipForge")
        self.set_default_size(1100, 760)
        self.config = load_config()

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_child(root)

        # Header bar
        self.header = Gtk.HeaderBar()
        title_label = Gtk.Label(label="ClipForge")
        title_label.add_css_class("title")
        self.header.set_title_widget(title_label)
        self.set_titlebar(self.header)

        menu_btn = Gtk.MenuButton(icon_name="open-menu-symbolic")
        menu = Gio.Menu()
        menu.append("Change clips folder...", "win.change_folder")
        menu.append("Open clips folder in file manager", "win.open_folder")
        menu.append("About ClipForge", "win.about")
        menu_btn.set_menu_model(menu)
        self.header.pack_end(menu_btn)

        self.folder_label = Gtk.Label(label=self.config["clips_dir"])
        self.folder_label.add_css_class("dim-label")
        self.folder_label.add_css_class("caption")
        self.folder_label.set_ellipsize(Pango.EllipsizeMode.MIDDLE)
        self.folder_label.set_max_width_chars(50)
        self.header.pack_start(self.folder_label)

        # Notification bar (created before stack so EditorView can call notify() during init)
        self.notify_revealer = Gtk.Revealer()
        self.notify_label = Gtk.Label()
        self.notify_label.set_margin_top(6)
        self.notify_label.set_margin_bottom(6)
        self.notify_revealer.set_child(self.notify_label)
        self.notify_revealer.set_transition_type(Gtk.RevealerTransitionType.SLIDE_UP)

        # Stack: library / editor
        self.stack = Gtk.Stack()
        self.stack.set_transition_type(Gtk.StackTransitionType.SLIDE_LEFT_RIGHT)
        self.library_view = LibraryView(self)
        self.editor_view = EditorView(self)
        self.stack.add_named(self.library_view, "library")
        self.stack.add_named(self.editor_view, "editor")
        root.append(self.stack)

        root.append(self.notify_revealer)

        # Actions
        change_action = Gio.SimpleAction.new("change_folder", None)
        change_action.connect("activate", self.on_change_folder)
        self.add_action(change_action)

        open_action = Gio.SimpleAction.new("open_folder", None)
        open_action.connect("activate", lambda a, p: self._open_folder(self.config["clips_dir"]))
        self.add_action(open_action)

        about_action = Gio.SimpleAction.new("about", None)
        about_action.connect("activate", self.on_about)
        self.add_action(about_action)

        self.refresh_library()

    def notify(self, text, sticky=False):
        self.notify_label.set_label(text)
        self.notify_revealer.set_reveal_child(True)
        if not sticky:
            def hide():
                self.notify_revealer.set_reveal_child(False)
                return False
            GLib.timeout_add(4000, hide)

    # -- Library management --

    def refresh_library(self):
        clips_dir = Path(self.config["clips_dir"])
        items = []
        if clips_dir.exists():
            for f in clips_dir.iterdir():
                if f.is_file() and f.suffix.lower() in VIDEO_EXTS:
                    try:
                        items.append(ClipItem(f))
                    except OSError:
                        continue
        self.library_view.set_items(items)

    def show_library(self):
        self.stack.set_visible_child_name("library")
        self.editor_view.player.stop()

    def open_editor(self, item: ClipItem):
        self.stack.set_visible_child_name("editor")
        self.editor_view.load_clip(item.path)

    def delete_clip(self, item: ClipItem):
        dialog = Gtk.MessageDialog(
            transient_for=self,
            modal=True,
            message_type=Gtk.MessageType.QUESTION,
            buttons=Gtk.ButtonsType.NONE,
            text="Delete clip?",
            secondary_text=f"\"{item.name}\" will be moved to the trash.",
        )
        dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
        delete_btn = dialog.add_button("Delete", Gtk.ResponseType.OK)
        delete_btn.add_css_class("destructive-action")

        def on_response(d, response):
            if response == Gtk.ResponseType.OK:
                try:
                    gfile = Gio.File.new_for_path(item.path)
                    gfile.trash(None)
                    self.notify(f"Deleted {item.name}")
                except GLib.Error as e:
                    self.notify(f"Could not delete: {e.message}")
                self.refresh_library()
            d.destroy()

        dialog.connect("response", on_response)
        dialog.present()

    def rename_clip(self, item: ClipItem):
        dialog = Gtk.Dialog(transient_for=self, modal=True, title="Rename clip")
        dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
        rename_btn = dialog.add_button("Rename", Gtk.ResponseType.OK)
        rename_btn.add_css_class("suggested-action")
        dialog.set_default_response(Gtk.ResponseType.OK)

        entry = Gtk.Entry()
        entry.set_text(item.name)
        entry.set_activates_default(True)
        entry.set_margin_top(8)
        entry.set_margin_bottom(8)
        entry.set_margin_start(8)
        entry.set_margin_end(8)
        content = dialog.get_content_area()
        content.append(entry)

        def on_response(d, response):
            if response == Gtk.ResponseType.OK:
                new_name = entry.get_text().strip()
                if new_name and new_name != item.name:
                    src = Path(item.path)
                    dst = src.with_name(new_name)
                    try:
                        src.rename(dst)
                        self.notify("Clip renamed")
                    except OSError as e:
                        self.notify(f"Could not rename: {e}")
                    self.refresh_library()
            d.destroy()

        dialog.connect("response", on_response)
        dialog.present()

    def show_in_folder(self, item: ClipItem):
        self._open_folder(str(Path(item.path).parent))

    def _open_folder(self, path):
        try:
            subprocess.Popen(["xdg-open", path])
        except Exception as e:
            self.notify(f"Could not open folder: {e}")

    # -- Settings --

    def on_change_folder(self, action, param):
        dialog = Gtk.FileDialog()
        dialog.set_title("Select clips folder")
        try:
            dialog.set_initial_folder(Gio.File.new_for_path(self.config["clips_dir"]))
        except Exception:
            pass

        def on_response(dlg, result):
            try:
                gfile = dlg.select_folder_finish(result)
            except GLib.Error:
                return
            if gfile:
                path = gfile.get_path()
                self.config["clips_dir"] = path
                save_config(self.config)
                self.folder_label.set_label(path)
                self.refresh_library()

        dialog.select_folder(self, None, on_response)

    def on_about(self, action, param):
        about = Gtk.AboutDialog(transient_for=self, modal=True)
        about.set_program_name("ClipForge")
        about.set_version("1.2")
        about.set_comments("A simple clip manager and trimmer for GPU Screen Recorder.")
        about.set_license_type(Gtk.License.MIT_X11)
        about.present()


class ClipForgeApp(Gtk.Application):
    def __init__(self):
        super().__init__(application_id=APP_ID)

    def do_activate(self):
        self._load_css()
        win = self.props.active_window
        if not win:
            win = ClipForgeWindow(self)
        win.present()

    def _load_css(self):
        css_path = Path(__file__).resolve().parent / "style.css"
        if css_path.exists():
            provider = Gtk.CssProvider()
            provider.load_from_path(str(css_path))
            Gtk.StyleContext.add_provider_for_display(
                Gdk.Display.get_default(), provider,
                Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
            )


def main():
    try:
        app = ClipForgeApp()
        return app.run(sys.argv)
    except Exception:
        traceback.print_exc()
        try:
            win = Gtk.Window(title="ClipForge - Error")
            win.set_default_size(500, 300)
            label = Gtk.Label(label="ClipForge failed to start:\n\n" + traceback.format_exc())
            label.set_wrap(True)
            label.set_margin_top(12)
            label.set_margin_bottom(12)
            label.set_margin_start(12)
            label.set_margin_end(12)
            win.set_child(label)
            win.connect("close-request", lambda w: sys.exit(1))
            win.present()
            app2 = Gtk.Application(application_id=APP_ID + ".error")
            app2.connect("activate", lambda a: None)
            return app2.run([])
        except Exception:
            return 1


if __name__ == "__main__":
    sys.exit(main())
