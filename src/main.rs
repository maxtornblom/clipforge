mod autoclip;
mod batch;
mod config;
mod editor;
mod library;
mod media;
mod overlay;
mod recorder;
mod settings;
mod timeline;
mod window;

use adw::prelude::*;
use gtk::{gdk, glib};

const APP_ID: &str = "se.local.clipforge";

/// `clipforge --batch-fullscreen`: convert every 4:3 clip without opening a window.
fn batch_fullscreen_cli() -> glib::ExitCode {
    use std::{path::PathBuf, sync::atomic::AtomicBool};
    let cfg = config::Config::load();
    let paths: Vec<PathBuf> = std::fs::read_dir(&cfg.clips_dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    let paths: Vec<PathBuf> = paths
        .into_iter()
        .filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            p.extension().and_then(|x| x.to_str()).is_some_and(|x| media::VIDEO_EXTS.contains(&x.to_lowercase().as_str()))
                && !name.contains(media::TMP_MARKER)
        })
        .collect();
    let found = batch::find_candidates(&paths);
    println!("{} 4:3 clip(s) to convert in {}", found.len(), cfg.clips_dir);
    batch::convert_all(&found, &AtomicBool::new(false), |m| match m {
        batch::Msg::Started { index, total, name } => {
            println!("[{index}/{total}] {name}");
        }
        batch::Msg::Progress(_) => {}
        batch::Msg::Finished { ok, failed, cancelled } => {
            println!("done: {ok} converted, {failed} failed{}", if cancelled { ", cancelled" } else { "" });
        }
    });
    glib::ExitCode::SUCCESS
}

/// `clipforge --save [SECONDS]`: save the replay buffer right now (bind this to a
/// hotkey). With SECONDS (10, 30, 60, 300, 600, 1800) only that much is saved;
/// without, the whole buffer. Only auto-clips are trimmed by ClipForge itself.
fn save_cli() -> glib::ExitCode {
    let secs = std::env::args().nth(2).and_then(|a| a.parse::<u32>().ok());
    let result = match secs {
        Some(s) => recorder::trigger_save_last(s),
        None => recorder::trigger_save(),
    };
    match result {
        // Saved: stay silent. A popup here only gets in the way mid-game.
        Ok(()) => {
            overlay::request_flash();
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("clipforge: {e}");
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "ClipForge", "-u", "critical", "Couldn't save clip", &e])
                .status();
            glib::ExitCode::FAILURE
        }
    }
}

fn main() -> glib::ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--save") {
        return save_cli();
    }
    if std::env::args().nth(1).as_deref() == Some("--batch-fullscreen") {
        return batch_fullscreen_cli();
    }
    if let Err(e) = gst::init() {
        eprintln!("clipforge: GStreamer failed to initialise: {e}");
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        gtk::Window::set_default_icon_name(APP_ID);
        // Medal-style: always dark, with a warm accent.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
    app.connect_activate(window::present);
    app.run()
}
