//! Auto-clipping. Counter-Strike 2 reports live game state (including your kill
//! count) to a local URL through its official Game State Integration feature.
//! We listen for that, and when your kills go up we ask the GPU Screen Recorder
//! replay buffer to save, after a short delay so the clip includes the aftermath.

use std::{
    cell::{Cell, RefCell},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    rc::Rc,
    thread,
    time::{Duration, Instant, SystemTime},
};

use adw::prelude::*;
use gtk::glib;

use crate::{batch, config::Config, recorder};

pub const GSI_PORT: u16 = 31777;
const CFG_NAME: &str = "gamestate_integration_clipforge.cfg";

// ---------------------------------------------------------------------------
// CS2 integration file
// ---------------------------------------------------------------------------

pub enum Cs2Cfg {
    NotFound,
    NotInstalled,
    Installed,
}

fn steam_roots() -> Vec<PathBuf> {
    let home = glib::home_dir();
    vec![
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ]
}

/// The `game/csgo/cfg` folder of the CS2 install, searching every Steam library.
fn find_cs2_cfg_dir() -> Option<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    for root in steam_roots() {
        if !root.is_dir() {
            continue;
        }
        libs.push(root.clone());
        if let Ok(vdf) = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            for line in vdf.lines() {
                let parts: Vec<&str> = line.split('"').collect();
                // "path"		"/some/library"
                if parts.len() >= 4 && parts[1] == "path" {
                    libs.push(PathBuf::from(parts[3]));
                }
            }
        }
    }
    libs.into_iter()
        .map(|l| l.join("steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg"))
        .find(|p| p.is_dir())
}

fn cfg_contents() -> String {
    format!(
        "\"ClipForge\"\n{{\n\
         \t\"uri\" \"http://127.0.0.1:{GSI_PORT}\"\n\
         \t\"timeout\" \"5.0\"\n\
         \t\"buffer\" \"0.1\"\n\
         \t\"throttle\" \"0.1\"\n\
         \t\"heartbeat\" \"10.0\"\n\
         \t\"data\"\n\t{{\n\
         \t\t\"provider\" \"1\"\n\
         \t\t\"map\" \"1\"\n\
         \t\t\"round\" \"1\"\n\
         \t\t\"player_id\" \"1\"\n\
         \t\t\"player_state\" \"1\"\n\
         \t\t\"player_match_stats\" \"1\"\n\
         \t}}\n}}\n"
    )
}

pub fn cs2_cfg_state() -> Cs2Cfg {
    match find_cs2_cfg_dir() {
        None => Cs2Cfg::NotFound,
        Some(dir) => {
            let file = dir.join(CFG_NAME);
            if fs::read_to_string(&file).is_ok_and(|s| s == cfg_contents()) {
                Cs2Cfg::Installed
            } else {
                Cs2Cfg::NotInstalled
            }
        }
    }
}

pub fn install_cs2_cfg() -> Result<PathBuf, String> {
    let dir = find_cs2_cfg_dir().ok_or("Counter-Strike 2 wasn't found in your Steam libraries")?;
    let file = dir.join(CFG_NAME);
    fs::write(&file, cfg_contents()).map_err(|e| format!("Could not write {}: {e}", file.display()))?;
    Ok(file)
}

// ---------------------------------------------------------------------------
// Local GSI server
// ---------------------------------------------------------------------------

/// What we need from one game-state update.
#[derive(Debug, PartialEq)]
pub struct GsiUpdate {
    /// The update describes the local player (not someone being spectated).
    pub local: bool,
    /// Kills in the whole match.
    pub kills: Option<u32>,
    /// Kills in the current round (5 = ace).
    pub round_kills: Option<u32>,
    pub map: Option<String>,
}

fn parse_update(body: &[u8]) -> Option<GsiUpdate> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let player = v.get("player")?;
    let local = match (v["provider"]["steamid"].as_str(), player["steamid"].as_str()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    let kills = player["match_stats"]["kills"].as_u64().map(|k| k as u32);
    let round_kills = player["state"]["round_kills"].as_u64().map(|k| k as u32);
    let map = v["map"]["name"].as_str().filter(|m| !m.is_empty()).map(str::to_owned);
    Some(GsiUpdate { local, kills, round_kills, map })
}

/// What to call a clip: an ace if the round's fifth kill is in it, else the streak size.
pub fn clip_label(kills: u32, round_kills: u32) -> (&'static str, String) {
    match kills {
        _ if round_kills >= 5 => ("👑", "Ace".into()),
        0 | 1 => ("🔫", "Kill".into()),
        2 => ("💥", "2 Kills".into()),
        3 => ("🔥", "3 Kills".into()),
        n => ("⚡", format!("{n} Kills")),
    }
}

/// "de_dust2" -> "Dust2", "cs_office" -> "Office". Only filename-safe characters survive.
fn pretty_map(map: &str) -> String {
    let base = ["de_", "cs_", "ar_", "dz_"].iter().find_map(|p| map.strip_prefix(p)).unwrap_or(map);
    let clean: String = base.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    clean
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Short, readable clip title: "🔥 3 Kills · Mirage".
pub fn clip_title(kills: u32, round_kills: u32, map: Option<&str>) -> String {
    let (emoji, label) = clip_label(kills, round_kills);
    match map.map(pretty_map).filter(|m| !m.is_empty()) {
        Some(m) => format!("{emoji} {label} · {m}"),
        None => format!("{emoji} {label}"),
    }
}

// ---------------------------------------------------------------------------
// Local GSI server
// ---------------------------------------------------------------------------

fn handle_conn(mut stream: TcpStream, tx: &async_channel::Sender<GsiUpdate>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if len > 2 * 1024 * 1024 {
        return;
    }
    while buf.len() < header_end + len {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    if let Some(u) = buf.get(header_end..).and_then(parse_update) {
        let _ = tx.send_blocking(u);
    }
}

/// Listen on localhost only. CS2 posts a fresh snapshot on every change.
pub fn start_server() -> Result<async_channel::Receiver<GsiUpdate>, String> {
    let listener = TcpListener::bind(("127.0.0.1", GSI_PORT))
        .map_err(|e| format!("Could not listen on port {GSI_PORT}: {e}"))?;
    let (tx, rx) = async_channel::unbounded();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || handle_conn(stream, &tx));
        }
    });
    Ok(rx)
}

// ---------------------------------------------------------------------------
// Kill-streak engine
// ---------------------------------------------------------------------------

struct Burst {
    kills: u32,
    started: Instant,
    round_kills: u32,
    map: Option<String>,
}

pub struct AutoClip {
    cfg: Rc<RefCell<Config>>,
    toasts: adw::ToastOverlay,
    server_started: Cell<bool>,
    last_kills: Cell<Option<u32>>,
    last_map: RefCell<Option<String>>,
    burst: RefCell<Option<Burst>>,
    /// Invalidates timers from earlier kills in the same streak.
    generation: Cell<u32>,
    on_saved: RefCell<Option<Box<dyn Fn()>>>,
}

impl AutoClip {
    pub fn new(cfg: Rc<RefCell<Config>>, toasts: adw::ToastOverlay) -> Rc<Self> {
        Rc::new(AutoClip {
            cfg,
            toasts,
            server_started: Cell::new(false),
            last_kills: Cell::new(None),
            last_map: RefCell::new(None),
            burst: RefCell::new(None),
            generation: Cell::new(0),
            on_saved: RefCell::new(None),
        })
    }

    /// Called the moment a save has been requested (used for the screen border).
    pub fn connect_saved(&self, f: impl Fn() + 'static) {
        *self.on_saved.borrow_mut() = Some(Box::new(f));
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    /// Start listening (once) if auto-clip is enabled.
    pub fn ensure_running(self: &Rc<Self>) {
        if !self.cfg.borrow().autoclip_cs2 || self.server_started.get() {
            return;
        }
        match start_server() {
            Ok(rx) => {
                self.server_started.set(true);
                let me = self.clone();
                glib::spawn_future_local(async move {
                    while let Ok(update) = rx.recv().await {
                        me.on_update(update);
                    }
                });
            }
            Err(e) => self.toast(&e),
        }
    }

    fn on_update(self: &Rc<Self>, u: GsiUpdate) {
        if !u.local {
            return; // spectating, or a menu snapshot
        }
        let Some(kills) = u.kills else { return };
        if let Some(b) = self.burst.borrow_mut().as_mut() {
            b.round_kills = b.round_kills.max(u.round_kills.unwrap_or(0));
            if u.map.is_some() {
                b.map = u.map.clone();
            }
        }
        if u.map.is_some() {
            *self.last_map.borrow_mut() = u.map.clone();
        }
        let prev = self.last_kills.replace(Some(kills));
        if !self.cfg.borrow().autoclip_cs2 {
            return;
        }
        // The first snapshot only sets the baseline; a lower count means a new match.
        if let Some(prev) = prev {
            if kills > prev {
                self.register_kills(kills - prev, u.round_kills.unwrap_or(0));
            }
        }
    }

    fn register_kills(self: &Rc<Self>, n: u32, round_kills: u32) {
        let (delay, replay_secs) = {
            let c = self.cfg.borrow();
            (c.clip_delay.max(1), c.replay_secs.max(15))
        };
        let age = {
            let mut b = self.burst.borrow_mut();
            let map = self.last_map.borrow().clone();
            let burst = b.get_or_insert(Burst { kills: 0, started: Instant::now(), round_kills: 0, map });
            burst.kills += n;
            burst.round_kills = burst.round_kills.max(round_kills);
            burst.started.elapsed().as_secs() as u32
        };
        let gen = self.generation.get() + 1;
        self.generation.set(gen);

        // Kills within `delay` seconds of each other become one clip, but never let
        // the streak outgrow the replay buffer or its first kill would fall off.
        let limit = replay_secs.saturating_sub(delay + 5);
        let wait = if age >= limit { 0 } else { delay.min(limit - age) };
        let me = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(wait as u64), move || {
            if me.generation.get() == gen {
                me.finish_burst();
            }
        });
    }

    fn finish_burst(self: &Rc<Self>) {
        let Some(burst) = self.burst.borrow_mut().take() else { return };
        let title = clip_title(burst.kills, burst.round_kills, burst.map.as_deref());
        // Clip length, but never shorter than the whole streak plus a little lead-in.
        let (length, delay) = {
            let c = self.cfg.borrow();
            (c.clip_length as f64, c.clip_delay as f64)
        };
        let span = burst.started.elapsed().as_secs_f64();
        let keep = length.max(span + delay + 8.0);
        self.save_now(Some(title.clone()), format!("{title} saved"), Some(keep));
    }

    /// Save the replay buffer; optionally rename the new file to `title`.
    pub fn save_now(self: &Rc<Self>, title: Option<String>, message: String, keep: Option<f64>) {
        let dir = PathBuf::from(self.cfg.borrow().clips_dir.clone());
        let since = SystemTime::now();
        if let Err(e) = recorder::trigger_save() {
            self.toast(&format!("Couldn't save clip: {e}. Start the replay buffer first."));
            return;
        }
        if let Some(f) = self.on_saved.borrow().as_ref() {
            f();
        }
        let me = self.clone();
        glib::spawn_future_local(async move {
            let Some(file) = recorder::await_new_clip(&dir, since).await else {
                me.toast("The clip was requested but no file appeared");
                return;
            };
            let file = match title {
                Some(t) => rename_with_title(&file, &t).unwrap_or(file),
                None => file,
            };
            let toast = adw::Toast::new(&message);
            toast.set_button_label(Some("Show"));
            toast.set_action_name(Some("win.clip-folder"));
            toast.set_action_target_value(Some(&file.to_string_lossy().to_variant()));
            me.toasts.add_toast(toast);

            // Trim to the configured length first (cheap, and less to re-encode), then
            // stretch 4:3 gameplay to fullscreen; the raw file is kept in Originals.
            let fill = me.cfg.borrow().autoclip_fill;
            if keep.is_some() || fill {
                let (tx, rx) = async_channel::bounded(1);
                let f = file.clone();
                std::thread::spawn(move || {
                    let trimmed = match keep {
                        Some(k) => batch::trim_to_last(&f, k),
                        None => Ok(false),
                    };
                    let filled = if fill { batch::fullscreen_in_place(&f) } else { Ok(false) };
                    let _ = tx.send_blocking((trimmed, filled));
                });
                if let Ok((trimmed, filled)) = rx.recv().await {
                    if let Err(e) = &trimmed {
                        me.toast(&format!("Couldn't trim the clip: {e}"));
                    }
                    match filled {
                        Ok(true) => me.toast("Made fullscreen (16:9). The original is in Originals"),
                        Err(e) => me.toast(&format!("Couldn't make the clip fullscreen: {e}")),
                        Ok(false) => {}
                    }
                }
            }
        });
    }
}

/// Rename "Replay_2026-11-12_20-31-02.mp4" to "🔥 3 Kills · Mirage · 20-31-02.mp4".
/// The time keeps names unique; the date is already shown on the clip card.
fn rename_with_title(file: &Path, title: &str) -> Option<PathBuf> {
    let stem = file.file_stem()?.to_string_lossy().into_owned();
    let ext = file.extension()?.to_string_lossy().into_owned();
    let time = stem.rsplit('_').next().filter(|t| t.len() == 8 && t.matches('-').count() == 2);
    let base = match time {
        Some(t) => format!("{title} · {t}"),
        None => format!("{title} · {stem}"),
    };
    let mut dst = file.with_file_name(format!("{base}.{ext}"));
    let mut n = 2;
    while dst.exists() {
        dst = file.with_file_name(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    fs::rename(file, &dst).ok()?;
    Some(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_player_kills() {
        let body = br#"{"provider":{"steamid":"1"},"player":{"steamid":"1","match_stats":{"kills":7}}}"#;
        assert_eq!(parse_update(body), Some(GsiUpdate { local: true, kills: Some(7), round_kills: None, map: None }));
    }

    #[test]
    fn parses_round_kills_and_map() {
        let body = br#"{"provider":{"steamid":"1"},"map":{"name":"de_mirage"},"player":{"steamid":"1","state":{"round_kills":5},"match_stats":{"kills":12}}}"#;
        let u = parse_update(body).unwrap();
        assert_eq!((u.round_kills, u.map.as_deref()), (Some(5), Some("de_mirage")));
    }

    #[test]
    fn titles() {
        assert_eq!(clip_title(1, 1, Some("de_mirage")), "🔫 Kill · Mirage");
        assert_eq!(clip_title(2, 2, Some("de_dust2")), "💥 2 Kills · Dust2");
        assert_eq!(clip_title(3, 3, Some("de_mirage")), "🔥 3 Kills · Mirage");
        assert_eq!(clip_title(4, 4, Some("cs_office")), "⚡ 4 Kills · Office");
        assert_eq!(clip_title(2, 5, Some("de_ancient")), "👑 Ace · Ancient"); // the round's fifth kill is in it
        assert_eq!(clip_title(5, 5, None), "👑 Ace");
        assert_eq!(clip_title(3, 2, Some("workshop_x")), "🔥 3 Kills · Workshop X");
        assert_eq!(clip_title(1, 1, Some("../x y")), "🔫 Kill · Xy");
    }

    #[test]
    fn rename_is_short_unique_and_safe() {
        let dir = std::env::temp_dir().join(format!("clipforge-rename-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("Replay_2026-11-12_20-31-02.mp4");
        fs::write(&a, b"x").unwrap();
        let out = rename_with_title(&a, "🔥 3 Kills · Mirage").unwrap();
        assert_eq!(out.file_name().unwrap().to_string_lossy(), "🔥 3 Kills · Mirage · 20-31-02.mp4");
        // Same title and second again: no overwrite.
        fs::write(&a, b"y").unwrap();
        let out2 = rename_with_title(&a, "🔥 3 Kills · Mirage").unwrap();
        assert_eq!(out2.file_name().unwrap().to_string_lossy(), "🔥 3 Kills · Mirage · 20-31-02 (2).mp4");
        assert_eq!(fs::read(&out).unwrap(), b"x");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_spectated_player() {
        let body = br#"{"provider":{"steamid":"1"},"player":{"steamid":"2","match_stats":{"kills":7}}}"#;
        assert_eq!(parse_update(body).unwrap().local, false);
    }

    #[test]
    fn menu_snapshot_has_no_player() {
        assert_eq!(parse_update(br#"{"provider":{"steamid":"1"}}"#), None);
    }

    #[test]
    fn server_receives_post() {
        // Real socket round trip against the same handler the server uses.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = async_channel::unbounded();
        thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            handle_conn(s, &tx);
        });
        let body = r#"{"provider":{"steamid":"9"},"player":{"steamid":"9","match_stats":{"kills":3}}}"#;
        let mut c = TcpStream::connect(addr).unwrap();
        write!(c, "POST / HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"));
        assert_eq!(rx.recv_blocking().unwrap(), GsiUpdate { local: true, kills: Some(3), round_kills: None, map: None });
    }
}
