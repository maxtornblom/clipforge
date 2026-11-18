//! Batch conversion: stretch every 4:3 clip in the library to fullscreen 16:9.
//! Originals are never modified; results are saved as `<name>_16x9.<ext>`.

use std::{
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
};

use adw::prelude::*;
use gtk::glib;

use crate::media::{self, ExportJob, Framing, LIBRARY_POOL};

const SUFFIX: &str = "_16x9";

fn output_path(src: &Path) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = src.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
    src.with_file_name(format!("{stem}{SUFFIX}.{ext}"))
}

/// 4:3 clips that haven't been converted yet (and aren't conversions themselves).
pub fn find_candidates(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|p| {
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            !stem.ends_with(SUFFIX) && !output_path(p).exists() && media::frame_size(p).is_some_and(media::is_four_three)
        })
        .cloned()
        .collect()
}

pub enum Msg {
    Started { index: usize, total: usize, name: String },
    Progress(f64),
    Finished { ok: usize, failed: usize, cancelled: bool },
}

/// Convert sequentially, reporting through `notify`. Runs on a worker thread.
pub fn convert_all(clips: &[PathBuf], cancel: &AtomicBool, notify: impl Fn(Msg)) {
    let (mut ok, mut failed) = (0, 0);
    for (i, src) in clips.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            notify(Msg::Finished { ok, failed, cancelled: true });
            return;
        }
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        notify(Msg::Started { index: i + 1, total: clips.len(), name });

        let dst = output_path(src);
        let tmp = media::temp_path_for(&dst);
        let duration = media::probe_duration(src);
        let Some(frame) = media::frame_size(src).filter(|_| duration > 0.0) else {
            failed += 1;
            continue;
        };
        let job = ExportJob { src: src.clone(), start: 0.0, duration, exact: true, framing: Framing::Fill, bars: None, frame };
        let result = media::run_export(&job, &tmp, cancel, |p| notify(Msg::Progress(p)))
            .and_then(|_| fs::rename(&tmp, &dst).map_err(|e| e.to_string()));
        match result {
            Ok(()) => ok += 1,
            Err(e) if e == "Cancelled" => {
                notify(Msg::Finished { ok, failed, cancelled: true });
                return;
            }
            Err(_) => {
                let _ = fs::remove_file(&tmp);
                failed += 1;
            }
        }
    }
    notify(Msg::Finished { ok, failed, cancelled: false });
}

/// Keep only the last `keep` seconds of a clip, in place and without re-encoding.
/// (Copy-mode cuts snap to a keyframe, so you get a little more, never less.)
/// Returns Ok(false) if the clip is already that short.
pub fn trim_to_last(file: &Path, keep: f64) -> Result<bool, String> {
    let duration = media::probe_duration(file);
    if duration <= 0.0 {
        return Err("couldn't read the clip length".into());
    }
    if duration <= keep + 1.0 {
        return Ok(false);
    }
    let tmp = media::temp_path_for(file);
    let job = ExportJob {
        src: file.to_owned(),
        start: duration - keep,
        duration: keep + 1.0,
        exact: false,
        framing: Framing::Original,
        bars: None,
        frame: (0, 0),
    };
    media::run_export(&job, &tmp, &AtomicBool::new(false), |_| {})?;
    fs::rename(&tmp, file).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(true)
}

/// Folder (inside the clips folder) that keeps raw originals after an in-place conversion.
pub const ORIGINALS_DIR: &str = "Originals";

/// Stretch a 4:3 clip to 16:9 *in place*: the converted file takes over the original's
/// path and the raw original is moved to `Originals/`. Returns Ok(false) if the clip
/// isn't 4:3 and was left alone. Blocking; run it off the main thread.
pub fn fullscreen_in_place(file: &Path) -> Result<bool, String> {
    let frame = media::frame_size(file).ok_or("couldn't read the video size")?;
    if !media::is_four_three(frame) {
        return Ok(false);
    }
    let duration = media::probe_duration(file);
    if duration <= 0.0 {
        return Err("couldn't read the clip length".into());
    }
    let tmp = media::temp_path_for(file);
    let job = ExportJob { src: file.to_owned(), start: 0.0, duration, exact: true, framing: Framing::Fill, bars: None, frame };
    media::run_export(&job, &tmp, &AtomicBool::new(false), |_| {})?;

    let dir = file.parent().ok_or("no parent folder")?.join(ORIGINALS_DIR);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = file.file_name().ok_or("no file name")?;
    let mut keep = dir.join(name);
    let mut n = 2;
    while keep.exists() {
        keep = dir.join(format!("{n}_{}", name.to_string_lossy()));
        n += 1;
    }
    fs::rename(file, &keep).map_err(|e| format!("couldn't keep the original: {e}"))?;
    if let Err(e) = fs::rename(&tmp, file) {
        let _ = fs::rename(&keep, file); // put the original back
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(true)
}

/// UI entry point: scan, confirm, then run with a progress dialog.
pub fn start(win: &adw::ApplicationWindow, library_paths: Vec<PathBuf>, toasts: adw::ToastOverlay, on_done: Rc<dyn Fn()>) {
    let win = win.clone();
    glib::spawn_future_local(async move {
        let found = LIBRARY_POOL.run(move || find_candidates(&library_paths)).await.unwrap_or_default();
        if found.is_empty() {
            toasts.add_toast(adw::Toast::new("No unconverted 4:3 clips found"));
            return;
        }
        let total_size: u64 = found.iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum();
        let confirm = adw::AlertDialog::new(
            Some(&format!("Make {} clip{} fullscreen?", found.len(), if found.len() == 1 { "" } else { "s" })),
            Some(&format!(
                "Each 4:3 clip is stretched to 16:9 and saved next to the original as “…{SUFFIX}”. \
                 Your originals aren't changed. Re-encoding {} of video can take a while.",
                media::human_size(total_size)
            )),
        );
        confirm.add_responses(&[("cancel", "Cancel"), ("go", "Convert")]);
        confirm.set_response_appearance("go", adw::ResponseAppearance::Suggested);
        confirm.set_close_response("cancel");
        if confirm.choose_future(Some(&win)).await != "go" {
            return;
        }

        let bar = gtk::ProgressBar::new();
        bar.set_show_text(true);
        let dialog = adw::AlertDialog::new(Some("Converting clips"), Some("Starting…"));
        dialog.set_extra_child(Some(&bar));
        dialog.add_response("cancel", "Cancel");
        dialog.set_close_response("cancel");
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            dialog.connect_response(None, move |_, _| cancel.store(true, Ordering::Relaxed));
        }
        dialog.present(Some(&win));

        let (tx, rx) = async_channel::unbounded::<Msg>();
        {
            let cancel = cancel.clone();
            std::thread::spawn(move || convert_all(&found, &cancel, |m| {
                let _ = tx.send_blocking(m);
            }));
        }
        let (mut index, mut total) = (0, 0);
        while let Ok(msg) = rx.recv().await {
            match msg {
                Msg::Started { index: i, total: t, name } => {
                    (index, total) = (i, t);
                    dialog.set_body(&format!("Clip {i} of {t}: {name}"));
                    bar.set_fraction((i - 1) as f64 / t as f64);
                    bar.set_text(Some(&format!("{} of {t} done", i - 1)));
                }
                Msg::Progress(p) => {
                    if total > 0 {
                        bar.set_fraction(((index - 1) as f64 + p) / total as f64);
                    }
                }
                Msg::Finished { ok, failed, cancelled } => {
                    dialog.force_close();
                    let mut text = format!("Converted {ok} clip{}", if ok == 1 { "" } else { "s" });
                    if failed > 0 {
                        text += &format!(", {failed} failed");
                    }
                    if cancelled {
                        text += " (cancelled)";
                    }
                    toasts.add_toast(adw::Toast::new(&text));
                    on_done();
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn make(path: &Path, size: &str) {
        assert!(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("smptebars=size={size}:rate=30:duration=3")])
            .args(["-f", "lavfi", "-i", "sine=duration=3", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(path)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn in_place_keeps_original_and_skips_16_9() {
        let dir = std::env::temp_dir().join(format!("clipforge-inplace-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("CS2 Ace - de_mirage - Replay_1.mp4"), dir.join("wide.mp4"));
        make(&a, "1440x1080");
        make(&b, "1920x1080");
        let before = fs::metadata(&b).unwrap().len();

        assert_eq!(fullscreen_in_place(&a), Ok(true));
        assert_eq!(media::frame_size(&a), Some((1920, 1080)), "clip at its own path is now fullscreen");
        let kept = dir.join(ORIGINALS_DIR).join("CS2 Ace - de_mirage - Replay_1.mp4");
        assert_eq!(media::frame_size(&kept), Some((1440, 1080)), "raw original kept");
        assert!(fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(media::TMP_MARKER)));

        assert_eq!(fullscreen_in_place(&b), Ok(false));
        assert_eq!(fs::metadata(&b).unwrap().len(), before, "16:9 clip must be untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trim_keeps_the_end_and_audio() {
        let dir = std::env::temp_dir().join(format!("clipforge-trim-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("long.mp4");
        assert!(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "smptebars=size=640x360:rate=30:duration=12", "-f", "lavfi", "-i", "sine=duration=12"])
            .args(["-g", "30", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&f).status().unwrap().success());
        assert_eq!(trim_to_last(&f, 5.0), Ok(true));
        let d = media::probe_duration(&f);
        assert!((5.0..=7.5).contains(&d), "duration {d}");
        let audio = Command::new("ffprobe").args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "csv=p=0"]).arg(&f).output().unwrap();
        assert!(!audio.stdout.is_empty(), "audio lost");
        // Already short enough: untouched.
        assert_eq!(trim_to_last(&f, 30.0), Ok(false));
        assert!(fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(media::TMP_MARKER)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aspect_rules() {
        assert!(media::is_four_three((1440, 1080)));
        assert!(media::is_four_three((1280, 960)));
        assert!(!media::is_four_three((1920, 1080)));
        assert!(!media::is_four_three((1080, 1080)));
    }

    #[test]
    fn converts_only_43_clips_and_keeps_originals() {
        let dir = std::env::temp_dir().join(format!("clipforge-batch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a43.mp4"), dir.join("b169.mp4"), dir.join("c43.mp4"));
        make(&a, "1440x1080");
        make(&b, "1920x1080");
        make(&c, "1280x960");
        let before = fs::metadata(&a).unwrap().len();

        let found = find_candidates(&[a.clone(), b.clone(), c.clone()]);
        assert_eq!(found, vec![a.clone(), c.clone()]);

        let events = std::cell::RefCell::new(vec![]);
        convert_all(&found, &AtomicBool::new(false), |m| {
            if let Msg::Finished { ok, failed, cancelled } = m {
                events.borrow_mut().push((ok, failed, cancelled));
            }
        });
        assert_eq!(*events.borrow(), vec![(2, 0, false)]);
        assert_eq!(media::frame_size(&dir.join("a43_16x9.mp4")), Some((1920, 1080)));
        assert_eq!(media::frame_size(&dir.join("c43_16x9.mp4")), Some((1706, 960)));
        assert_eq!(fs::metadata(&a).unwrap().len(), before, "original modified");
        assert!(fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(media::TMP_MARKER)));

        // Re-running skips what's already converted, and never re-converts outputs.
        let again = find_candidates(&[a, b, c, dir.join("a43_16x9.mp4")]);
        assert!(again.is_empty(), "{again:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
