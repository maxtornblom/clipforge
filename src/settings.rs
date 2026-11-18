//! Auto-clip & replay-buffer settings dialog.

use std::{cell::RefCell, rc::Rc, time::Duration};

use adw::prelude::*;
use gtk::glib;

use crate::{
    autoclip::{self, AutoClip, Cs2Cfg},
    config::Config,
    recorder::{Recorder, Status},
};

/// Start the ClipForge-run replay buffer and report a failure to start.
pub fn start_managed(recorder: &Rc<Recorder>, cfg: &Rc<RefCell<Config>>, toasts: &adw::ToastOverlay) {
    if let Err(e) = recorder.start(&cfg.borrow()) {
        toasts.add_toast(adw::Toast::new(&e));
        return;
    }
    let (recorder, toasts) = (recorder.clone(), toasts.clone());
    glib::timeout_add_local_once(Duration::from_millis(2000), move || {
        if let Some(why) = recorder.startup_failure() {
            toasts.add_toast(adw::Toast::new(&format!("Replay buffer stopped: {why}")));
        }
    });
}

fn save<F: FnOnce(&mut Config)>(cfg: &Rc<RefCell<Config>>, f: F) {
    let mut c = cfg.borrow_mut();
    f(&mut c);
    c.save();
}

fn cs2_subtitle(state: &Cs2Cfg) -> (&'static str, &'static str) {
    match state {
        Cs2Cfg::NotFound => ("Counter-Strike 2 wasn't found in your Steam libraries.", ""),
        Cs2Cfg::NotInstalled => ("Not installed yet. Install it, then restart CS2 once.", "Install"),
        Cs2Cfg::Installed => ("Installed. CS2 reports your kills to ClipForge.", "Reinstall"),
    }
}

pub fn show(
    win: &adw::ApplicationWindow,
    cfg: Rc<RefCell<Config>>,
    recorder: Rc<Recorder>,
    auto: Rc<AutoClip>,
    toasts: adw::ToastOverlay,
) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Auto-Clip");
    let page = adw::PreferencesPage::new();

    // -- Auto-clip ----------------------------------------------------------
    let group = adw::PreferencesGroup::new();
    group.set_title("Auto-Clip");
    group.set_description(Some("Save a clip the moment something good happens."));

    let cs2 = adw::SwitchRow::new();
    cs2.set_title("Counter-Strike 2 kills");
    cs2.set_subtitle("Saves a clip when you get a kill. Kills in quick succession become one clip.");
    cs2.set_active(cfg.borrow().autoclip_cs2);
    {
        let (cfg, auto) = (cfg.clone(), auto.clone());
        cs2.connect_active_notify(move |r| {
            save(&cfg, |c| c.autoclip_cs2 = r.is_active());
            auto.ensure_running();
        });
    }
    group.add(&cs2);

    let integ = adw::ActionRow::new();
    integ.set_title("CS2 integration");
    let state = autoclip::cs2_cfg_state();
    let (sub, label) = cs2_subtitle(&state);
    integ.set_subtitle(sub);
    let install = gtk::Button::with_label(label);
    install.set_valign(gtk::Align::Center);
    install.set_visible(!label.is_empty());
    if matches!(state, Cs2Cfg::NotInstalled) {
        install.add_css_class("suggested-action");
    }
    {
        let integ = integ.clone();
        install.connect_clicked(move |b| match autoclip::install_cs2_cfg() {
            Ok(_) => {
                integ.set_subtitle("Installed. Restart CS2 once for it to take effect.");
                b.set_label("Reinstall");
                b.remove_css_class("suggested-action");
            }
            Err(e) => integ.set_subtitle(&e),
        });
    }
    integ.add_suffix(&install);
    group.add(&integ);

    let delay = adw::SpinRow::with_range(1.0, 15.0, 1.0);
    delay.set_title("Keep after the last kill");
    delay.set_subtitle("Seconds of footage to include after the kill");
    delay.set_value(cfg.borrow().clip_delay as f64);
    {
        let cfg = cfg.clone();
        delay.connect_value_notify(move |r| save(&cfg, |c| c.clip_delay = r.value() as u32));
    }
    group.add(&delay);

    let length = adw::SpinRow::with_range(10.0, 120.0, 5.0);
    length.set_title("Clip length");
    length.set_subtitle("Auto-clips only: seconds per clip, ending just after the kill. Long streaks and aces are kept whole. Clips you save yourself always use the full buffer length.");
    length.set_value(cfg.borrow().clip_length as f64);
    {
        let cfg = cfg.clone();
        length.connect_value_notify(move |r| save(&cfg, |c| c.clip_length = r.value() as u32));
    }
    group.add(&length);

    let fill_new = adw::SwitchRow::new();
    fill_new.set_title("Fullscreen new 4:3 clips");
    fill_new.set_subtitle("Stretches new 4:3 clips to 16:9 after saving. Raw originals go to an “Originals” folder.");
    fill_new.set_active(cfg.borrow().autoclip_fill);
    {
        let cfg = cfg.clone();
        fill_new.connect_active_notify(move |r| save(&cfg, |c| c.autoclip_fill = r.is_active()));
    }
    group.add(&fill_new);
    page.add(&group);

    let border = adw::SwitchRow::new();
    border.set_title("Screen border on save");
    border.set_subtitle("A gold border flashes around the screen for a moment whenever a clip is saved.");
    border.set_active(cfg.borrow().flash_border);
    {
        let cfg = cfg.clone();
        border.connect_active_notify(move |r| save(&cfg, |c| c.flash_border = r.is_active()));
    }
    group.add(&border);

    // -- Editor -------------------------------------------------------------
    let ed = adw::PreferencesGroup::new();
    ed.set_title("Editor");
    let fill = adw::SwitchRow::new();
    fill.set_title("Fill screen for 4:3 clips");
    fill.set_subtitle("Open 4:3 clips stretched to fullscreen 16:9. You can still switch back per clip.");
    fill.set_active(cfg.borrow().fill_43_default);
    {
        let cfg = cfg.clone();
        fill.connect_active_notify(move |r| save(&cfg, |c| c.fill_43_default = r.is_active()));
    }
    ed.add(&fill);
    page.add(&ed);

    // -- Replay buffer ------------------------------------------------------
    let rb = adw::PreferencesGroup::new();
    rb.set_title("Replay Buffer");
    rb.set_description(Some("Auto-clip saves from GPU Screen Recorder's replay buffer, so it must be running."));

    let status = adw::ActionRow::new();
    status.set_title("Status");
    let refresh_status = {
        let (status, recorder) = (status.clone(), recorder.clone());
        move || {
            status.set_subtitle(match recorder.status() {
                Status::External => "Running (started by GPU Screen Recorder)",
                Status::Managed => "Running (started by ClipForge)",
                Status::Stopped => "Not running",
            })
        }
    };
    refresh_status();
    let test = gtk::Button::with_label("Save Clip Now");
    test.set_valign(gtk::Align::Center);
    {
        let auto = auto.clone();
        test.connect_clicked(move |_| auto.save_now(None, "Clip saved".into(), None));
    }
    status.add_suffix(&test);
    rb.add(&status);

    let managed = adw::SwitchRow::new();
    managed.set_title("Run the replay buffer with ClipForge");
    managed.set_subtitle("Starts with ClipForge, using your GPU Screen Recorder monitor and audio settings.");
    managed.set_active(cfg.borrow().managed_replay);
    {
        let (cfg, recorder, toasts) = (cfg.clone(), recorder.clone(), toasts.clone());
        let refresh = refresh_status.clone();
        managed.connect_active_notify(move |r| {
            save(&cfg, |c| c.managed_replay = r.is_active());
            if r.is_active() {
                start_managed(&recorder, &cfg, &toasts);
            } else {
                recorder.stop();
            }
            let refresh = refresh.clone();
            glib::timeout_add_local_once(Duration::from_millis(600), move || refresh());
        });
    }
    rb.add(&managed);

    let secs = adw::SpinRow::with_range(15.0, 300.0, 5.0);
    secs.set_title("Buffer length");
    secs.set_subtitle("Seconds kept in memory, and the length of clips you save yourself. Applies the next time ClipForge starts the buffer.");
    secs.set_value(cfg.borrow().replay_secs as f64);
    {
        let cfg = cfg.clone();
        secs.connect_value_notify(move |r| save(&cfg, |c| c.replay_secs = r.value() as u32));
    }
    rb.add(&secs);
    page.add(&rb);

    dialog.add(&page);
    dialog.present(Some(win));
}
