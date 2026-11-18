//! Main window: library page, editor page, actions, folder watching.

use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{
    autoclip::AutoClip,
    batch,
    config::Config,
    editor::Editor,
    library::{Library, Sort},
    overlay::Flash,
    recorder::{Recorder, Status},
    settings,
};

struct Ui {
    win: adw::ApplicationWindow,
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    cfg: Rc<RefCell<Config>>,
    library: Rc<Library>,
    editor: Rc<Editor>,
    recorder: Rc<Recorder>,
    auto: Rc<AutoClip>,
    monitor: RefCell<Option<gio::FileMonitor>>,
    refresh_pending: Cell<bool>,
}

pub fn present(app: &adw::Application) {
    if let Some(w) = app.active_window() {
        w.present();
        return;
    }

    let cfg = Rc::new(RefCell::new(Config::load()));
    let toasts = adw::ToastOverlay::new();
    let nav = adw::NavigationView::new();
    let library = Library::new(cfg.clone());
    let editor = Editor::new(cfg.clone(), toasts.clone());
    let recorder = Rc::new(Recorder::new());
    let auto = AutoClip::new(cfg.clone(), toasts.clone());

    // -- Library page header ------------------------------------------------
    let brand = gtk::Label::new(Some("ClipForge"));
    brand.add_css_class("brand");

    let all_btn = gtk::ToggleButton::with_label("Clips");
    let fav_btn = gtk::ToggleButton::with_label("Favorites");
    fav_btn.set_group(Some(&all_btn));
    all_btn.set_active(true);
    let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tabs.add_css_class("linked");
    tabs.add_css_class("tabs");
    tabs.append(&all_btn);
    tabs.append(&fav_btn);

    let search_btn = gtk::ToggleButton::new();
    search_btn.set_icon_name("system-search-symbolic");
    search_btn.set_tooltip_text(Some("Search (Ctrl+F)"));

    let sort_menu = gio::Menu::new();
    for (label, id) in [("Newest first", "newest"), ("Oldest first", "oldest"), ("Name", "name"), ("Largest first", "size")] {
        sort_menu.append(Some(label), Some(&format!("win.sort::{id}")));
    }
    let sort_btn = gtk::MenuButton::new();
    sort_btn.set_icon_name("view-sort-descending-symbolic");
    sort_btn.set_tooltip_text(Some("Sort"));
    sort_btn.set_menu_model(Some(&sort_menu));

    let main_menu = gio::Menu::new();
    main_menu.append(Some("Auto-Clip Settings…"), Some("win.autoclip-settings"));
    main_menu.append(Some("Change Clips Folder…"), Some("win.change-folder"));
    main_menu.append(Some("Make 4:3 Clips Fullscreen…"), Some("win.batch-fullscreen"));
    main_menu.append(Some("Open Clips Folder"), Some("win.open-folder"));
    main_menu.append(Some("Refresh"), Some("win.refresh"));
    main_menu.append(Some("About ClipForge"), Some("win.about"));
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("open-menu-symbolic");
    menu_btn.set_menu_model(Some(&main_menu));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&tabs));
    header.pack_start(&brand);
    let clip_now = gtk::Button::from_icon_name("media-record-symbolic");
    clip_now.set_tooltip_text(Some("Save a clip now (replay buffer)"));
    clip_now.set_action_name(Some("win.clip-now"));
    clip_now.add_css_class("clip-now");
    header.pack_end(&menu_btn);
    header.pack_end(&clip_now);
    header.pack_end(&sort_btn);
    header.pack_end(&search_btn);

    let search_entry = gtk::SearchEntry::new();
    search_entry.set_hexpand(true);
    search_entry.set_placeholder_text(Some("Search clips"));
    let search_bar = gtk::SearchBar::new();
    search_bar.set_child(Some(&search_entry));
    search_bar.connect_entry(&search_entry);
    search_btn.bind_property("active", &search_bar, "search-mode-enabled").bidirectional().sync_create().build();

    let lib_view = adw::ToolbarView::new();
    lib_view.add_top_bar(&header);
    lib_view.add_top_bar(&search_bar);
    lib_view.set_content(Some(&library.widget));
    search_bar.set_key_capture_widget(Some(&lib_view));

    let lib_page = adw::NavigationPage::new(&lib_view, "ClipForge");
    lib_page.set_tag(Some("library"));
    nav.add(&lib_page);
    toasts.set_child(Some(&nav));

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("ClipForge")
        .default_width(1320)
        .default_height(840)
        .maximized(true)
        .content(&toasts)
        .build();

    let ui = Rc::new(Ui {
        win: win.clone(),
        nav: nav.clone(),
        toasts,
        cfg: cfg.clone(),
        library: library.clone(),
        editor: editor.clone(),
        recorder: recorder.clone(),
        auto: auto.clone(),
        monitor: RefCell::new(None),
        refresh_pending: Cell::new(false),
    });

    // -- Header wiring ------------------------------------------------------
    {
        let lib = library.clone();
        fav_btn.connect_toggled(move |b| lib.set_favs_only(b.is_active()));
    }
    {
        let lib = library.clone();
        search_entry.connect_search_changed(move |e| lib.set_query(&e.text()));
    }
    {
        let ui2 = ui.clone();
        library.connect_open(move |clip| ui2.open_editor(&clip.path, clip.duration.get()));
    }
    {
        let ui2 = ui.clone();
        editor.connect_changed(move || ui2.library.refresh());
    }

    // -- Actions ------------------------------------------------------------
    add_path_action(&ui, "clip-open", |ui, path| {
        if let Some(c) = ui.library.clip_for(path) {
            ui.open_editor(&c.path, c.duration.get());
        }
    });
    add_path_action(&ui, "clip-favorite", |ui, path| ui.library.toggle_favorite(path));
    add_path_action(&ui, "clip-copy", |ui, path| ui.copy_file(PathBuf::from(path)));
    add_path_action(&ui, "clip-fullscreen", |ui, path| ui.make_fullscreen(PathBuf::from(path)));
    add_path_action(&ui, "clip-folder", |ui, path| ui.show_in_folder(PathBuf::from(path)));
    add_path_action(&ui, "clip-rename", |ui, path| ui.clone().rename(PathBuf::from(path)));
    add_path_action(&ui, "clip-delete", |ui, path| ui.clone().delete(PathBuf::from(path)));

    add_action(&win, "change-folder", {
        let ui = ui.clone();
        move || ui.clone().change_folder()
    });
    add_action(&win, "open-folder", {
        let ui = ui.clone();
        move || ui.open_folder()
    });
    add_action(&win, "refresh", {
        let lib = library.clone();
        move || lib.refresh()
    });
    add_action(&win, "clip-now", {
        let ui = ui.clone();
        move || ui.auto.save_now(None, "Clip saved".into(), None)
    });
    add_action(&win, "autoclip-settings", {
        let ui = ui.clone();
        move || settings::show(&ui.win, ui.cfg.clone(), ui.recorder.clone(), ui.auto.clone(), ui.toasts.clone())
    });
    app.set_accels_for_action("win.autoclip-settings", &["<Ctrl>comma"]);
    add_action(&win, "batch-fullscreen", {
        let ui = ui.clone();
        move || {
            let lib = ui.library.clone();
            batch::start(&ui.win, ui.library.all_paths(), ui.toasts.clone(), Rc::new(move || lib.refresh()));
        }
    });
    add_action(&win, "about", {
        let ui = ui.clone();
        move || ui.about()
    });
    add_action(&win, "search", {
        let b = search_btn.clone();
        move || b.set_active(!b.is_active())
    });
    app.set_accels_for_action("win.search", &["<Ctrl>f"]);
    app.set_accels_for_action("win.refresh", &["F5"]);

    let sort = gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &"newest".to_variant());
    {
        let lib = library.clone();
        sort.connect_activate(move |a, p| {
            if let Some(id) = p.and_then(|p| p.str()) {
                a.set_state(&id.to_variant());
                lib.set_sort(Sort::from_id(id));
            }
        });
    }
    win.add_action(&sort);

    // -- Editor shortcuts (window-wide, not just when a widget has focus) ---
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let ui = ui.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            if !ui.editor_visible() {
                return glib::Propagation::Proceed;
            }
            // Let spin buttons receive typing; only Space etc. are ours otherwise.
            if gtk::prelude::GtkWindowExt::focus(&ui.win).is_some_and(|w| w.is::<gtk::Text>()) {
                return glib::Propagation::Proceed;
            }
            if ui.editor.handle_key(key, state & gdk::ModifierType::SHIFT_MASK) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    win.add_controller(keys);

    ui.watch_folder();
    library.refresh();
    win.present();

    // Screen border: flashes whenever a clip is saved (auto, button or F8).
    if let Some(flash) = Flash::new() {
        let target = || {
            let w = crate::recorder::gsr_ui_defaults().window;
            (!matches!(w.as_str(), "screen" | "focused" | "portal")).then_some(w)
        };
        let show = {
            let (flash, cfg) = (flash.clone(), cfg.clone());
            move || {
                if cfg.borrow().flash_border {
                    flash.pulse(target().as_deref());
                }
            }
        };
        let action = gio::SimpleAction::new("flash", None);
        {
            let show = show.clone();
            action.connect_activate(move |_, _| show());
        }
        app.add_action(&action);
        auto.connect_saved(show);
    }

    // Auto-clip: listen for game events, and make sure a replay buffer exists.
    auto.ensure_running();
    if cfg.borrow().managed_replay {
        settings::start_managed(&recorder, &cfg, &ui.toasts);
    } else if cfg.borrow().autoclip_cs2 {
        let ui2 = ui.clone();
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            if ui2.recorder.status() == Status::Stopped {
                ui2.toast("Auto-clip is on, but the replay buffer isn't running");
            }
        });
    }
    let rec = recorder.clone();
    app.connect_shutdown(move |_| rec.stop());
}

fn add_action(win: &adw::ApplicationWindow, name: &str, f: impl Fn() + 'static) {
    let a = gio::SimpleAction::new(name, None);
    a.connect_activate(move |_, _| f());
    win.add_action(&a);
}

fn add_path_action(ui: &Rc<Ui>, name: &str, f: impl Fn(&Rc<Ui>, &str) + 'static) {
    let a = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
    let ui2 = Rc::downgrade(ui);
    a.connect_activate(move |_, p| {
        if let (Some(ui), Some(path)) = (ui2.upgrade(), p.and_then(|p| p.str())) {
            f(&ui, path);
        }
    });
    ui.win.add_action(&a);
}

impl Ui {
    fn editor_visible(&self) -> bool {
        self.nav.visible_page().and_then(|p| p.tag()).as_deref() == Some("editor")
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn open_editor(&self, path: &Path, duration: f64) {
        self.editor.load(path, duration);
        if !self.editor_visible() {
            self.nav.push(&self.editor.page);
        }
    }

    /// Watch the clips folder so new replays appear without a manual refresh.
    fn watch_folder(self: &Rc<Self>) {
        let dir = PathBuf::from(self.cfg.borrow().clips_dir.clone());
        *self.monitor.borrow_mut() = None;
        let file = gio::File::for_path(&dir);
        let Ok(mon) = file.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) else {
            return;
        };
        let weak = Rc::downgrade(self);
        mon.connect_changed(move |_, _, _, _| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.refresh_pending.replace(true) {
                return;
            }
            // Debounce: a recorder writing a clip fires many events.
            glib::timeout_add_local_once(Duration::from_millis(700), move || {
                ui.refresh_pending.set(false);
                ui.library.refresh();
            });
        });
        *self.monitor.borrow_mut() = Some(mon);
    }

    fn change_folder(self: Rc<Self>) {
        glib::spawn_future_local(async move {
            let dialog = gtk::FileDialog::builder().title("Select clips folder").build();
            dialog.set_initial_folder(Some(&gio::File::for_path(&self.cfg.borrow().clips_dir)));
            let Ok(file) = dialog.select_folder_future(Some(&self.win)).await else {
                return;
            };
            if let Some(path) = file.path() {
                {
                    let mut cfg = self.cfg.borrow_mut();
                    cfg.clips_dir = path.to_string_lossy().into_owned();
                    cfg.save();
                }
                self.watch_folder();
                self.library.refresh();
            }
        });
    }

    fn open_folder(&self) {
        let file = gio::File::for_path(&self.cfg.borrow().clips_dir);
        let launcher = gtk::FileLauncher::new(Some(&file));
        let win = self.win.clone();
        glib::spawn_future_local(async move {
            let _ = launcher.launch_future(Some(&win)).await;
        });
    }

    /// Put the clip on the clipboard as a file, so it can be pasted into a chat or
    /// file manager (e.g. straight into Discord).
    fn copy_file(&self, path: PathBuf) {
        let Some(display) = gdk::Display::default() else { return };
        let list = gdk::FileList::from_array(&[gio::File::for_path(&path)]);
        let provider = gdk::ContentProvider::for_value(&list.to_value());
        match display.clipboard().set_content(Some(&provider)) {
            Ok(()) => self.toast("Copied. Paste it into a chat or folder"),
            Err(e) => self.toast(&format!("Couldn't copy: {e}")),
        }
    }

    /// Save a 16:9 stretched copy of one 4:3 clip next to the original.
    fn make_fullscreen(self: &Rc<Self>, path: PathBuf) {
        let found = batch::find_candidates(&[path.clone()]);
        if found.is_empty() {
            self.toast("Already converted, or not a 4:3 clip");
            return;
        }
        self.toast("Making it fullscreen…");
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            batch::convert_all(&found, &std::sync::atomic::AtomicBool::new(false), |m| {
                if let batch::Msg::Finished { ok, failed, .. } = m {
                    let _ = tx.send_blocking((ok, failed));
                }
            });
        });
        let ui = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok((1, _)) => {
                    ui.toast("Saved a fullscreen copy next to the original");
                    ui.library.refresh();
                }
                _ => ui.toast("Couldn't convert that clip"),
            }
        });
    }

    fn show_in_folder(&self, path: PathBuf) {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
        let win = self.win.clone();
        glib::spawn_future_local(async move {
            if launcher.open_containing_folder_future(Some(&win)).await.is_err() {
                if let Some(parent) = path.parent() {
                    let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
                }
            }
        });
    }

    fn rename(self: Rc<Self>, path: PathBuf) {
        let old_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let stem_len = path.file_stem().map(|s| s.to_string_lossy().chars().count()).unwrap_or(0) as i32;

        let entry = gtk::Entry::new();
        entry.set_text(&old_name);
        entry.set_activates_default(true);
        let dialog = adw::AlertDialog::new(Some("Rename Clip"), None);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        glib::idle_add_local_once({
            let entry = entry.clone();
            move || {
                entry.grab_focus();
                entry.select_region(0, stem_len);
            }
        });

        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&self.win)).await != "rename" {
                return;
            }
            let mut new_name = entry.text().trim().to_owned();
            if new_name.is_empty() || new_name == old_name {
                return;
            }
            if new_name.contains('/') || new_name.contains('\0') || new_name == "." || new_name == ".." {
                self.toast("A clip name can't contain \"/\"");
                return;
            }
            // Keep the extension if the user typed only a new stem.
            if Path::new(&new_name).extension().is_none() {
                if let Some(ext) = path.extension() {
                    new_name = format!("{new_name}.{}", ext.to_string_lossy());
                }
            }
            let dst = path.with_file_name(&new_name);
            if dst.exists() {
                self.toast("A clip with that name already exists");
                return;
            }
            match fs::rename(&path, &dst) {
                Ok(()) => {
                    {
                        let mut cfg = self.cfg.borrow_mut();
                        cfg.rename_favorite(&path.to_string_lossy(), &dst.to_string_lossy());
                        cfg.save();
                    }
                    self.toast("Clip renamed");
                    self.library.refresh();
                }
                Err(e) => self.toast(&format!("Could not rename: {e}")),
            }
        });
    }

    fn delete(self: Rc<Self>, path: PathBuf) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some("Move Clip to Trash?"), Some(&format!("“{name}” will be moved to the trash.")));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Move to Trash")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        glib::spawn_future_local(async move {
            if dialog.choose_future(Some(&self.win)).await != "delete" {
                return;
            }
            match gio::File::for_path(&path).trash(gio::Cancellable::NONE) {
                Ok(()) => {
                    {
                        let mut cfg = self.cfg.borrow_mut();
                        cfg.remove_favorite(&path.to_string_lossy());
                        cfg.save();
                    }
                    self.toast(&format!("Moved {name} to trash"));
                    self.library.refresh();
                }
                Err(e) => self.toast(&format!("Could not delete: {}", e.message())),
            }
        });
    }

    fn about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("ClipForge")
            .developer_name("ClipForge")
            .version(env!("CARGO_PKG_VERSION"))
            .comments("A fast clip manager and trimmer for GPU Screen Recorder.")
            .license_type(gtk::License::MitX11)
            .build();
        about.present(Some(&self.win));
    }
}
