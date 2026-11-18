//! Everything that shells out to ffmpeg/ffprobe, plus the small worker pools
//! that keep those calls from piling up (the Python version spawned one thread
//! per clip, per refresh).

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock,
    },
};

use gtk::glib;

pub const VIDEO_EXTS: [&str; 4] = ["mp4", "mkv", "webm", "mov"];
pub const TMP_MARKER: &str = ".clipforge_tmp";

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

pub fn human_size(bytes: u64) -> String {
    let mut v = bytes as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if unit == "B" || unit == "KB" {
                format!("{v:.0} {unit}")
            } else {
                format!("{v:.1} {unit}")
            };
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

pub fn human_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Like `human_duration` but with tenths, for trim points.
pub fn precise_duration(secs: f64) -> String {
    let tenths = (secs.max(0.0) * 10.0).round() as u64;
    format!("{}.{}", human_duration((tenths / 10) as f64), tenths % 10)
}

pub fn relative_date(mtime: std::time::SystemTime) -> String {
    let Ok(age) = std::time::SystemTime::now().duration_since(mtime) else {
        return "Just now".into();
    };
    let s = age.as_secs();
    match s {
        0..=59 => "Just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        86400..=172799 => "Yesterday".into(),
        _ if s < 86400 * 30 => format!("{} days ago", s / 86400),
        _ => {
            let dt = glib::DateTime::from_unix_local((mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)) as i64);
            dt.ok()
                .and_then(|d| d.format("%b %-d, %Y").ok())
                .map(|g| g.to_string())
                .unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------------------
// Worker pools
// ---------------------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send>;

pub struct Pool(async_channel::Sender<Job>);

impl Pool {
    fn new(threads: usize) -> Self {
        let (tx, rx) = async_channel::unbounded::<Job>();
        for _ in 0..threads {
            let rx = rx.clone();
            std::thread::spawn(move || {
                while let Ok(job) = rx.recv_blocking() {
                    job();
                }
            });
        }
        Pool(tx)
    }

    /// Run `f` on the pool and await its result from the GLib main loop.
    pub async fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = async_channel::bounded(1);
        self.0
            .try_send(Box::new(move || {
                let _ = tx.send_blocking(f());
            }))
            .ok()?;
        rx.recv().await.ok()
    }
}

/// Library work: durations and thumbnails for every clip.
pub static LIBRARY_POOL: LazyLock<Pool> = LazyLock::new(|| Pool::new(4));
/// Editor work: filmstrip and scrub frames. Separate so opening a clip never
/// waits behind a backlog of thumbnails.
pub static EDITOR_POOL: LazyLock<Pool> = LazyLock::new(|| Pool::new(2));

// ---------------------------------------------------------------------------
// Probing and thumbnails
// ---------------------------------------------------------------------------

pub fn file_key(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{name}-{mtime}-{}", m.len())
        }
        Err(_) => name,
    }
}

fn cache_path(sub: &str, key: &str, ext: &str) -> PathBuf {
    let hash = glib::compute_checksum_for_string(glib::ChecksumType::Sha1, key)
        .map(|g| g.to_string())
        .unwrap_or_else(|| "unknown".into());
    let dir = glib::user_cache_dir().join("clipforge").join(sub);
    let _ = fs::create_dir_all(&dir);
    dir.join(format!("{hash}.{ext}"))
}

pub fn probe_duration(path: &Path) -> f64 {
    Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .filter(|d| d.is_finite())
        .unwrap_or(0.0)
}

fn ffmpeg_still(path: &Path, ts: f64, scale: &str, quality: &str, out: &Path) -> bool {
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-ss", &format!("{ts:.3}"), "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", scale, "-q:v", quality])
        .arg(out)
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok && out.exists()
}

pub fn ensure_thumbnail(path: &Path, duration: f64) -> Option<PathBuf> {
    let out = cache_path("thumbs-v2", &file_key(path), "jpg");
    if out.exists() {
        return Some(out);
    }
    // Clips shorter than 2s would have no frame at t=1.
    let ts = if duration > 0.0 { (duration / 2.0).min(1.0) } else { 0.0 };
    ffmpeg_still(path, ts, "scale=480:-2", "4", &out).then_some(out)
}

/// One ffmpeg call producing a horizontal strip of `count` frames
/// (the Python version ran `count` separate ffmpeg processes).
pub fn ensure_filmstrip(path: &Path, duration: f64, count: u32) -> Option<PathBuf> {
    if duration <= 0.0 {
        return None;
    }
    let out = cache_path("filmstrips", &format!("{}-{count}", file_key(path)), "jpg");
    if out.exists() {
        return Some(out);
    }
    let vf = format!("fps={count}/{duration:.3},scale=-2:96,tile={count}x1");
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-skip_frame", "nokey", "-i"])
        .arg(path)
        .args(["-an", "-vf", &vf, "-frames:v", "1", "-q:v", "5"])
        .arg(&out)
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && out.exists() {
        return Some(out);
    }
    // Keyframe-only decode can come up short on odd files; retry plainly.
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-an", "-vf", &vf, "-frames:v", "1", "-q:v", "5"])
        .arg(&out)
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    (ok && out.exists()).then_some(out)
}

/// Single frame for the no-GStreamer-sink fallback preview.
pub fn scrub_frame(path: &Path, ts: f64) -> Option<PathBuf> {
    let key = format!("{}-{:.1}", file_key(path), ts);
    let out = cache_path("scrub", &key, "jpg");
    if out.exists() {
        return Some(out);
    }
    ffmpeg_still(path, ts, "scale=960:-2", "3", &out).then_some(out)
}

// ---------------------------------------------------------------------------
// Black-bar detection (e.g. 4:3 gameplay pillarboxed inside a 16:9 recording)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crop {
    pub w: u32,
    pub h: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarInfo {
    pub frame: (u32, u32),
    pub content: Crop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Framing {
    /// Keep the recording exactly as captured.
    Original,
    /// Remove any black bars and stretch the picture to fill a 16:9 frame
    /// (the "stretched" look: 4:3 gameplay becomes fullscreen).
    Fill,
    /// Remove the bars and keep the picture's true aspect ratio.
    Crop,
}

pub fn frame_size(path: &Path) -> Option<(u32, u32)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (w, h) = text.trim().split_once(',')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn parse_crop(stderr: &str) -> Option<Crop> {
    let line = stderr.lines().rev().find(|l| l.contains("crop="))?;
    let spec = &line[line.rfind("crop=")? + 5..];
    let n: Vec<u32> = spec.split_whitespace().next()?.split(':').filter_map(|v| v.parse().ok()).collect();
    (n.len() == 4).then(|| Crop { w: n[0], h: n[1], x: n[2], y: n[3] })
}

/// Sample a few moments and take the union of what ffmpeg's cropdetect finds:
/// a dark scene can fake a bar, but real picture content can't be missed everywhere.
/// Returns `Some` only when bars of a meaningful size exist.
pub fn detect_bars(path: &Path, duration: f64) -> Option<BarInfo> {
    let (fw, fh) = frame_size(path)?;
    let points: Vec<f64> = if duration > 6.0 { vec![0.2, 0.5, 0.8].iter().map(|f| f * duration).collect() } else { vec![0.0] };
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut any = false;
    for t in points {
        let out = Command::new("ffmpeg")
            .args(["-v", "info", "-nostdin", "-ss", &format!("{t:.2}"), "-i"])
            .arg(path)
            .args(["-t", "2", "-an", "-vf", "cropdetect=limit=24:round=2:reset=0", "-f", "null", "-"])
            .output()
            .ok()?;
        if let Some(c) = parse_crop(&String::from_utf8_lossy(&out.stderr)) {
            any = true;
            x0 = x0.min(c.x);
            y0 = y0.min(c.y);
            x1 = x1.max(c.x + c.w);
            y1 = y1.max(c.y + c.h);
        }
    }
    if !any || x1 <= x0 || y1 <= y0 {
        return None;
    }
    // Keep dimensions even so the encoder accepts them.
    let content = Crop { x: x0 & !1, y: y0 & !1, w: (x1 - x0) & !1, h: (y1 - y0) & !1 };
    let bars = content.w + 8 < fw || content.h + 8 < fh;
    let sane = content.w as u64 * content.h as u64 * 4 >= fw as u64 * fh as u64;
    (bars && sane).then_some(BarInfo { frame: (fw, fh), content })
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

pub struct ExportJob {
    pub src: PathBuf,
    pub start: f64,
    pub duration: f64,
    pub exact: bool,
    pub framing: Framing,
    pub bars: Option<BarInfo>,
    /// Size of the source video frame.
    pub frame: (u32, u32),
}

/// 4:3 (and 5:4) frames, but not square ones.
pub fn is_four_three(frame: (u32, u32)) -> bool {
    frame.1 > 0 && (1.2..=1.4).contains(&(frame.0 as f64 / frame.1 as f64))
}

/// True when the frame is already (close to) 16:9.
pub fn is_16_9(frame: (u32, u32)) -> bool {
    frame.1 > 0 && ((frame.0 as f64 / frame.1 as f64) - 16.0 / 9.0).abs() < 0.02
}

/// The 16:9 size a stretched clip is scaled to: same height, widened.
pub fn fill_size(frame: (u32, u32)) -> (u32, u32) {
    let w = (frame.1 as f64 * 16.0 / 9.0).round() as u32;
    (w.max(frame.0) & !1, frame.1 & !1)
}

/// Where an export should be written: a hidden-ish temp file next to the target
/// so the library never lists a half-written clip, then renamed into place.
pub fn temp_path_for(dst: &Path) -> PathBuf {
    let stem = dst.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = dst.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
    dst.with_file_name(format!("{stem}{TMP_MARKER}.{ext}"))
}

pub fn unique_clip_path(src: &Path) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = src.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
    let mut n = 1;
    loop {
        let suffix = if n == 1 { "_clip".to_string() } else { format!("_clip{n}") };
        let candidate = src.with_file_name(format!("{stem}{suffix}.{ext}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Run ffmpeg for `job`, writing to `tmp`. Reports progress as 0.0..=1.0.
/// Maps every video and audio stream: GPU Screen Recorder often writes game and
/// mic audio as separate tracks, and ffmpeg's default mapping keeps only one.
pub fn run_export(job: &ExportJob, tmp: &Path, cancel: &AtomicBool, progress: impl Fn(f64)) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y", "-nostdin", "-progress", "pipe:1", "-nostats"])
        .args(["-ss", &format!("{:.3}", job.start)])
        .arg("-i")
        .arg(&job.src)
        .args(["-t", &format!("{:.3}", job.duration)])
        .args(["-map", "0:v:0", "-map", "0:a?"]);
    // Reframing changes pixels, so it always re-encodes.
    let crop = job
        .bars
        .map(|b| format!("crop={}:{}:{}:{}", b.content.w, b.content.h, b.content.x, b.content.y));
    let reframe = match job.framing {
        Framing::Original => None,
        Framing::Crop => crop.map(|c| format!("{c},setsar=1")),
        Framing::Fill => {
            let (w, h) = fill_size(job.frame);
            let scale = format!("scale={w}:{h},setsar=1");
            match crop {
                Some(c) => Some(format!("{c},{scale}")),
                None if is_16_9(job.frame) => None,
                None => Some(scale),
            }
        }
    };
    if let Some(vf) = &reframe {
        cmd.args(["-vf", vf]);
    }
    if job.exact || reframe.is_some() {
        cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-c:a", "aac", "-b:a", "192k"]);
    } else {
        cmd.args(["-c", "copy", "-avoid_negative_ts", "make_zero"]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(tmp);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());

    let mut child = cmd.spawn().map_err(|e| format!("Could not start ffmpeg: {e}"))?;
    let mut stderr = child.stderr.take().expect("piped");
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let stdout = child.stdout.take().expect("piped");
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(tmp);
            return Err("Cancelled".into());
        }
        if let Some(us) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = us.trim().parse::<f64>() {
                progress((us / 1e6 / job.duration).clamp(0.0, 1.0));
            }
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    let err = err_thread.join().unwrap_or_default();
    if status.success() && tmp.exists() {
        Ok(())
    } else {
        let _ = fs::remove_file(tmp);
        let tail: String = err.trim().lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        Err(if tail.is_empty() { "ffmpeg failed".into() } else { tail })
    }
}

pub fn cancel_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streams(p: &Path) -> (usize, usize) {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(p)
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        (s.lines().filter(|l| l.trim() == "video").count(), s.lines().filter(|l| l.trim() == "audio").count())
    }

    /// Exports must keep every audio track (GSR writes game + mic separately).
    #[test]
    fn export_keeps_all_audio_tracks() {
        let dir = std::env::temp_dir().join(format!("clipforge-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30:duration=6"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=6", "-f", "lavfi", "-i", "sine=frequency=880:duration=6"])
            .args(["-map", "0", "-map", "1", "-map", "2", "-g", "30", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&src)
            .status()
            .unwrap()
            .success();
        assert!(ok, "could not build test clip");
        assert_eq!(streams(&src), (1, 2));

        for exact in [false, true] {
            let dst = dir.join(format!("out_{exact}.mp4"));
            let tmp = temp_path_for(&dst);
            let job = ExportJob { src: src.clone(), start: 1.5, duration: 3.0, exact, framing: Framing::Original, bars: None, frame: (0, 0) };
            let last = std::cell::Cell::new(0.0);
            run_export(&job, &tmp, &AtomicBool::new(false), |p| last.set(p)).unwrap();
            fs::rename(&tmp, &dst).unwrap();
            assert_eq!(streams(&dst), (1, 2), "exact={exact}: tracks lost");
            let d = probe_duration(&dst);
            let tol = if exact { 0.2 } else { 1.5 }; // copy snaps to keyframes
            assert!((d - 3.0).abs() < tol, "exact={exact}: duration {d}");
            assert!(last.get() > 0.9, "progress never reached the end: {}", last.get());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn dims(p: &Path) -> (u32, u32) {
        frame_size(p).unwrap()
    }

    /// 4:3 gameplay pillarboxed in a 16:9 recording: detect the bars, then fill or crop.
    #[test]
    fn reframing_removes_black_bars() {
        let dir = std::env::temp_dir().join(format!("clipforge-bars-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("bars.mp4");
        assert!(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "smptebars=size=1440x1080:rate=30:duration=8", "-f", "lavfi", "-i", "sine=duration=8"])
            .args(["-vf", "pad=1920:1080:240:0:black", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&src).status().unwrap().success());
        assert_eq!(dims(&src), (1920, 1080));

        let bars = detect_bars(&src, 8.0).expect("bars should be detected");
        assert_eq!((bars.content.w, bars.content.h, bars.content.x), (1440, 1080, 240));

        for (framing, want) in [(Framing::Fill, (1920, 1080)), (Framing::Crop, (1440, 1080))] {
            let dst = dir.join(format!("out_{framing:?}.mp4"));
            let tmp = temp_path_for(&dst);
            let job = ExportJob { src: src.clone(), start: 0.0, duration: 4.0, exact: false, framing, bars: Some(bars), frame: (1920, 1080) };
            run_export(&job, &tmp, &AtomicBool::new(false), |_| {}).unwrap();
            assert_eq!(dims(&tmp), want, "{framing:?}");
            // Fill must leave no bars: re-detecting finds none.
            if framing == Framing::Fill {
                assert!(detect_bars(&tmp, 4.0).is_none(), "bars remain after Fill");
            }
        }
        // Native 4:3 clip (like GSR's 1440x1080 recordings): no bars, Fill stretches it to 16:9.
        let native = dir.join("native.mp4");
        assert!(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "smptebars=size=1440x1080:rate=30:duration=4", "-f", "lavfi", "-i", "sine=duration=4"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&native).status().unwrap().success());
        assert!(detect_bars(&native, 4.0).is_none());
        assert!(!is_16_9(frame_size(&native).unwrap()));
        let dst = dir.join("native_fill.mp4");
        let tmp = temp_path_for(&dst);
        let job = ExportJob { src: native.clone(), start: 0.0, duration: 3.0, exact: false, framing: Framing::Fill, bars: None, frame: (1440, 1080) };
        run_export(&job, &tmp, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(dims(&tmp), (1920, 1080));
        assert_eq!(streams(&tmp), (1, 1), "audio must survive reframing");

        // A clip without bars is not flagged.
        let plain = dir.join("plain.mp4");
        assert!(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "smptebars=size=1920x1080:rate=30:duration=4", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&plain).status().unwrap().success());
        assert!(detect_bars(&plain, 4.0).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_removes_temp() {
        let dir = std::env::temp_dir().join(format!("clipforge-cancel-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("s.mp4");
        assert!(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=20", "-c:v", "libx264"])
            .arg(&src).status().unwrap().success());
        let tmp = temp_path_for(&dir.join("o.mp4"));
        let job = ExportJob { src, start: 0.0, duration: 20.0, exact: true, framing: Framing::Original, bars: None, frame: (0, 0) };
        let r = run_export(&job, &tmp, &AtomicBool::new(true), |_| {});
        assert_eq!(r, Err("Cancelled".to_string()));
        assert!(!tmp.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn formatting() {
        assert_eq!(human_duration(65.0), "1:05");
        assert_eq!(human_duration(3725.0), "1:02:05");
        assert_eq!(precise_duration(3.46), "0:03.5");
        assert_eq!(human_size(1536), "2 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
