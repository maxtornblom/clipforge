//! GPU Screen Recorder control: find or start a replay buffer, save it on demand,
//! and pick up the file it writes.

use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

use gtk::glib;

use crate::{config::Config, media};

#[derive(Clone, Copy, PartialEq)]
pub enum Status {
    /// A replay buffer started elsewhere (e.g. GPU Screen Recorder's own UI).
    External,
    /// A replay buffer ClipForge started.
    Managed,
    Stopped,
}

/// PIDs of running `gpu-screen-recorder` processes in replay mode (`-r`).
pub fn find_replay_pids() -> Vec<u32> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    let mut pids = vec![];
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let comm = fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        if !comm.trim().starts_with("gpu-screen-rec") {
            continue;
        }
        let Ok(raw) = fs::read(e.path().join("cmdline")) else { continue };
        if raw.split(|b| *b == 0).any(|a| a == b"-r") {
            pids.push(pid);
        }
    }
    pids
}

/// Real-time signal GPU Screen Recorder uses to save only the last `secs` seconds.
fn last_secs_signal(secs: u32) -> Option<&'static str> {
    Some(match secs {
        10 => "RTMIN+1",
        30 => "RTMIN+2",
        60 => "RTMIN+3",
        300 => "RTMIN+4",
        600 => "RTMIN+5",
        1800 => "RTMIN+6",
        _ => return None,
    })
}

/// Save the last `secs` seconds (10, 30, 60, 300, 600 or 1800) of the replay buffer.
pub fn trigger_save_last(secs: u32) -> Result<(), String> {
    let sig = last_secs_signal(secs).ok_or("Supported lengths: 10, 30, 60, 300, 600, 1800 seconds")?;
    send_signal(sig)
}

/// Ask every running replay buffer to save its whole buffer (SIGUSR1).
pub fn trigger_save() -> Result<(), String> {
    send_signal("USR1")
}

fn send_signal(sig: &str) -> Result<(), String> {
    let pids = find_replay_pids();
    if pids.is_empty() {
        return Err("The replay buffer isn't running".into());
    }
    for pid in pids {
        let ok = Command::new("kill").args(["-s", sig, &pid.to_string()]).status().is_ok_and(|s| s.success());
        if !ok {
            return Err("Could not signal GPU Screen Recorder".into());
        }
    }
    Ok(())
}

/// Only GPU Screen Recorder's own replay files count. Anything else that happens to
/// be written to the folder (batch conversions, exports) must never be mistaken for it.
fn is_replay_file(name: &str) -> bool {
    name.starts_with("Replay_") && !name.contains("_16x9") && !name.contains(media::TMP_MARKER)
}

/// Wait for the clip GPU Screen Recorder writes after a save signal: the newest
/// `Replay_…` video in `dir` modified since `since` whose size has stopped changing.
pub async fn await_new_clip(dir: &Path, since: SystemTime) -> Option<PathBuf> {
    let since = since.checked_sub(Duration::from_secs(1)).unwrap_or(since);
    let mut previous: Option<(PathBuf, u64)> = None;
    for _ in 0..24 {
        glib::timeout_future(Duration::from_millis(500)).await;
        let newest = fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let ext_ok = p
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| media::VIDEO_EXTS.contains(&x.to_lowercase().as_str()));
                let meta = e.metadata().ok()?;
                let m = meta.modified().ok()?;
                (ext_ok && m >= since && is_replay_file(&e.file_name().to_string_lossy()))
                    .then_some((p, meta.len(), m))
            })
            .max_by_key(|(_, _, m)| *m);
        if let Some((p, len, _)) = newest {
            if previous.as_ref() == Some(&(p.clone(), len)) && len > 0 {
                return Some(p);
            }
            previous = Some((p, len));
        }
    }
    None
}

/// Defaults read from GPU Screen Recorder's own UI config, so a ClipForge-run
/// buffer records the same monitor and audio as the one you already use.
pub struct GsrDefaults {
    pub window: String,
    pub fps: u32,
    pub audio: Vec<String>,
}

pub fn gsr_ui_defaults() -> GsrDefaults {
    let mut d = GsrDefaults { window: "screen".into(), fps: 60, audio: vec!["default_output".into()] };
    let path = glib::user_config_dir().join("gpu-screen-recorder").join("config_ui");
    let Some(v) = fs::read_to_string(path).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else {
        return d;
    };
    let opts = &v["replay"]["record_options"];
    if let Some(area) = opts["record_area_option"].as_str() {
        d.window = match area {
            "focused_monitor" => "focused".into(),
            "screen" | "" => "screen".into(),
            other => other.to_owned(),
        };
    }
    if let Some(f) = opts["fps"].as_u64().filter(|f| *f > 0) {
        d.fps = f as u32;
    }
    if let Some(items) = opts["audio_track_item"][0]["audio_inputs"].as_array() {
        let a: Vec<String> = items
            .iter()
            .filter_map(|i| i.as_str())
            .map(|s| s.strip_prefix("device:").unwrap_or(s).to_owned())
            .collect();
        if !a.is_empty() {
            d.audio = a;
        }
    }
    d
}

pub struct Recorder {
    child: RefCell<Option<Child>>,
}

impl Recorder {
    pub fn new() -> Self {
        Recorder { child: RefCell::new(None) }
    }

    pub fn status(&self) -> Status {
        let mut slot = self.child.borrow_mut();
        if let Some(c) = slot.as_mut() {
            match c.try_wait() {
                Ok(None) => return Status::Managed,
                _ => *slot = None,
            }
        }
        drop(slot);
        if find_replay_pids().is_empty() {
            Status::Stopped
        } else {
            Status::External
        }
    }

    /// Start our own replay buffer unless one is already running.
    pub fn start(&self, cfg: &Config) -> Result<(), String> {
        if self.status() != Status::Stopped {
            return Ok(());
        }
        let d = gsr_ui_defaults();
        let log_dir = glib::user_cache_dir().join("clipforge");
        let _ = fs::create_dir_all(&log_dir);
        let log = fs::File::create(log_dir.join("gpu-screen-recorder.log")).map_err(|e| e.to_string())?;
        let child = Command::new("gpu-screen-recorder")
            .args(["-w", &d.window, "-c", "mp4", "-f", &d.fps.to_string()])
            .args(["-a", &d.audio.join("|")])
            .args(["-q", "very_high", "-r", &cfg.replay_secs.to_string()])
            .args(["-replay-storage", "ram", "-o", &cfg.clips_dir])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("Could not start gpu-screen-recorder: {e}"))?;
        *self.child.borrow_mut() = Some(child);
        Ok(())
    }

    /// If it dies right after starting, return the tail of its log.
    pub fn startup_failure(&self) -> Option<String> {
        if self.status() == Status::Managed {
            return None;
        }
        let log = glib::user_cache_dir().join("clipforge").join("gpu-screen-recorder.log");
        let text = fs::read_to_string(log).unwrap_or_default();
        let tail: Vec<&str> = text.trim().lines().rev().take(2).collect();
        Some(if tail.is_empty() { "it exited immediately".into() } else { tail.into_iter().rev().collect::<Vec<_>>().join(" ") })
    }

    /// Stop only the buffer ClipForge started, never an external one.
    pub fn stop(&self) {
        let Some(mut child) = self.child.borrow_mut().take() else { return };
        let _ = Command::new("kill").args(["-INT", &child.id().to_string()]).status();
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_files_that_are_not_gsr_replays() {
        let dir = std::env::temp_dir().join(format!("clipforge-await-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let since = SystemTime::now();
        fs::write(dir.join("Replay_2026-09-18_23-42-56_16x9.mp4"), b"converted").unwrap();
        fs::write(dir.join("Replay_x.clipforge_tmp.mp4"), b"tmp").unwrap();
        fs::write(dir.join("export_clip.mp4"), b"export").unwrap();
        fs::write(dir.join("Replay_2026-11-12_20-31-02.mp4"), b"the real clip").unwrap();
        let found = glib::MainContext::default().block_on(await_new_clip(&dir, since));
        assert_eq!(found.unwrap().file_name().unwrap().to_string_lossy(), "Replay_2026-11-12_20-31-02.mp4");

        let only_others = std::env::temp_dir().join(format!("clipforge-await2-{}", std::process::id()));
        fs::create_dir_all(&only_others).unwrap();
        fs::write(only_others.join("Replay_a_16x9.mp4"), b"x").unwrap();
        assert!(glib::MainContext::default().block_on(await_new_clip(&only_others, since)).is_none());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&only_others);
    }
}
