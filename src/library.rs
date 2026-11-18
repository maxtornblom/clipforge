//! The clip grid: Medal-style cards with thumbnails, favorites, search and sort.
//! Cards are built once per file and kept; filter/sort only re-evaluate, so
//! typing in the search box never rebuilds or re-probes anything.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs,
    path::PathBuf,
    rc::Rc,
    time::SystemTime,
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{
    config::Config,
    media::{self, LIBRARY_POOL},
};

pub struct Clip {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mtime: SystemTime,
    pub key: String,
    pub duration: Cell<f64>,
    /// Video frame size, filled in once probed.
    pub frame: Cell<Option<(u32, u32)>>,
}

struct Card {
    child: gtk::FlowBoxChild,
    clip: Rc<Clip>,
    fav: gtk::Button,
    popover: gtk::PopoverMenu,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Sort {
    Newest,
    Oldest,
    Name,
    Size,
}

impl Sort {
    pub fn from_id(id: &str) -> Self {
        match id {
            "oldest" => Sort::Oldest,
            "name" => Sort::Name,
            "size" => Sort::Size,
            _ => Sort::Newest,
        }
    }
}

pub struct Library {
    pub widget: gtk::Stack,
    cfg: Rc<RefCell<Config>>,
    flow: gtk::FlowBox,
    summary: gtk::Label,
    status: adw::StatusPage,
    status_btn: gtk::Button,
    cards: RefCell<HashMap<String, Card>>,
    durations: RefCell<HashMap<String, f64>>,
    query: RefCell<String>,
    sort: Cell<Sort>,
    favs_only: Cell<bool>,
    folder_missing: Cell<bool>,
    on_open: RefCell<Option<Box<dyn Fn(Rc<Clip>)>>>,
}

impl Library {
    pub fn new(cfg: Rc<RefCell<Config>>) -> Rc<Self> {
        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::None);
        flow.set_homogeneous(true);
        flow.set_min_children_per_line(1);
        flow.set_max_children_per_line(8);
        flow.set_row_spacing(18);
        flow.set_column_spacing(18);
        flow.set_valign(gtk::Align::Start);
        flow.set_margin_start(24);
        flow.set_margin_end(24);
        flow.set_margin_bottom(24);
        flow.set_margin_top(6);

        let summary = gtk::Label::new(None);
        summary.add_css_class("dim-label");
        summary.set_xalign(0.0);
        summary.set_margin_start(26);
        summary.set_margin_top(10);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_vexpand(true);
        scroller.set_child(Some(&flow));
        let grid = gtk::Box::new(gtk::Orientation::Vertical, 0);
        grid.append(&summary);
        grid.append(&scroller);

        let status = adw::StatusPage::new();
        status.set_icon_name(Some("video-x-generic-symbolic"));
        let status_btn = gtk::Button::with_label("Choose Clips Folder");
        status_btn.add_css_class("pill");
        status_btn.add_css_class("suggested-action");
        status_btn.set_halign(gtk::Align::Center);
        status_btn.set_action_name(Some("win.change-folder"));
        status.set_child(Some(&status_btn));

        let widget = gtk::Stack::new();
        widget.add_named(&grid, Some("grid"));
        widget.add_named(&status, Some("status"));

        let lib = Rc::new(Library {
            widget,
            cfg,
            flow,
            summary,
            status,
            status_btn,
            cards: RefCell::default(),
            durations: RefCell::default(),
            query: RefCell::default(),
            sort: Cell::new(Sort::Newest),
            favs_only: Cell::new(false),
            folder_missing: Cell::new(false),
            on_open: RefCell::new(None),
        });

        // Filter / sort read the clip behind each child through its widget name.
        let weak = Rc::downgrade(&lib);
        lib.flow.set_filter_func(move |child| {
            weak.upgrade()
                .and_then(|l| l.clip_of(child).map(|c| l.matches(&c)))
                .unwrap_or(true)
        });
        let weak = Rc::downgrade(&lib);
        lib.flow.set_sort_func(move |a, b| {
            let Some(l) = weak.upgrade() else { return gtk::Ordering::Equal };
            let (Some(a), Some(b)) = (l.clip_of(a), l.clip_of(b)) else {
                return gtk::Ordering::Equal;
            };
            let ord = match l.sort.get() {
                Sort::Newest => b.mtime.cmp(&a.mtime),
                Sort::Oldest => a.mtime.cmp(&b.mtime),
                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Sort::Size => b.size.cmp(&a.size),
            };
            ord.into()
        });

        let weak = Rc::downgrade(&lib);
        lib.flow.connect_child_activated(move |_, child| {
            if let Some(l) = weak.upgrade() {
                if let Some(clip) = l.clip_of(child) {
                    if let Some(f) = l.on_open.borrow().as_ref() {
                        f(clip);
                    }
                }
            }
        });
        lib
    }

    pub fn connect_open(&self, f: impl Fn(Rc<Clip>) + 'static) {
        *self.on_open.borrow_mut() = Some(Box::new(f));
    }

    pub fn all_paths(&self) -> Vec<PathBuf> {
        self.cards.borrow().values().map(|c| c.clip.path.clone()).collect()
    }

    pub fn clip_for(&self, path: &str) -> Option<Rc<Clip>> {
        self.cards.borrow().get(path).map(|c| c.clip.clone())
    }

    fn clip_of(&self, child: &gtk::FlowBoxChild) -> Option<Rc<Clip>> {
        self.cards.borrow().get(child.widget_name().as_str()).map(|c| c.clip.clone())
    }

    fn matches(&self, clip: &Clip) -> bool {
        let q = self.query.borrow();
        if !q.is_empty() && !clip.name.to_lowercase().contains(q.as_str()) {
            return false;
        }
        !self.favs_only.get() || self.cfg.borrow().is_favorite(&clip.path.to_string_lossy())
    }

    // -- View state ---------------------------------------------------------

    pub fn set_query(&self, q: &str) {
        *self.query.borrow_mut() = q.trim().to_lowercase();
        self.flow.invalidate_filter();
        self.update_view();
    }

    pub fn set_sort(&self, s: Sort) {
        self.sort.set(s);
        self.flow.invalidate_sort();
    }

    pub fn set_favs_only(&self, on: bool) {
        self.favs_only.set(on);
        self.flow.invalidate_filter();
        self.update_view();
    }

    fn update_view(&self) {
        let cards = self.cards.borrow();
        let total = cards.len();
        let shown: Vec<_> = cards.values().filter(|c| self.matches(&c.clip)).collect();

        if shown.is_empty() {
            let query_active = !self.query.borrow().is_empty();
            let (title, desc, btn) = if self.folder_missing.get() {
                (
                    "Clips Folder Not Found",
                    format!("{} doesn't exist. Pick the folder GPU Screen Recorder saves to.", self.cfg.borrow().clips_dir),
                    true,
                )
            } else if total == 0 {
                (
                    "No Clips Yet",
                    format!("New recordings in {} will show up here automatically.", self.cfg.borrow().clips_dir),
                    true,
                )
            } else if query_active {
                ("No Results", "No clips match your search.".to_owned(), false)
            } else {
                ("No Favorites Yet", "Tap the star on a clip to keep it here.".to_owned(), false)
            };
            self.status.set_title(title);
            self.status.set_description(Some(&desc));
            self.status_btn.set_visible(btn);
            self.widget.set_visible_child_name("status");
            return;
        }

        let bytes: u64 = shown.iter().map(|c| c.clip.size).sum();
        self.summary.set_label(&format!(
            "{} clip{} · {}",
            shown.len(),
            if shown.len() == 1 { "" } else { "s" },
            media::human_size(bytes)
        ));
        self.widget.set_visible_child_name("grid");
    }

    pub fn toggle_favorite(&self, path: &str) {
        let on = {
            let mut cfg = self.cfg.borrow_mut();
            let on = cfg.toggle_favorite(path);
            cfg.save();
            on
        };
        if let Some(card) = self.cards.borrow().get(path) {
            set_fav_look(&card.fav, on);
        }
        self.flow.invalidate_filter();
        self.update_view();
    }

    // -- Scanning -----------------------------------------------------------

    pub fn refresh(self: &Rc<Self>) {
        let dir = PathBuf::from(self.cfg.borrow().clips_dir.clone());
        self.folder_missing.set(!dir.is_dir());

        let mut found: HashMap<String, PathBuf> = HashMap::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                let is_video = p
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| media::VIDEO_EXTS.contains(&x.to_lowercase().as_str()));
                if is_video && !name.contains(media::TMP_MARKER) && p.is_file() {
                    found.insert(p.to_string_lossy().into_owned(), p);
                }
            }
        }

        // Drop cards for vanished or modified files (a modified file gets a new card
        // so its thumbnail and duration are regenerated).
        let stale: Vec<String> = self
            .cards
            .borrow()
            .iter()
            .filter(|(k, c)| !found.contains_key(*k) || media::file_key(&c.clip.path) != c.clip.key)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            let card = self.cards.borrow_mut().remove(&k);
            if let Some(card) = card {
                card.popover.unparent();
                self.flow.remove(&card.child);
            }
        }

        for (key, path) in found {
            if self.cards.borrow().contains_key(&key) {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else { continue };
            let clip = Rc::new(Clip {
                name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                size: meta.len(),
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                key: media::file_key(&path),
                duration: Cell::new(0.0),
                frame: Cell::new(None),
                path,
            });
            self.add_card(clip);
        }
        self.update_view();
    }

    fn add_card(self: &Rc<Self>, clip: Rc<Clip>) {
        let path_str = clip.path.to_string_lossy().into_owned();

        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_size_request(280, 158);
        picture.add_css_class("clip-thumb");

        let badge = gtk::Label::new(None);
        badge.add_css_class("duration-badge");
        badge.set_halign(gtk::Align::End);
        badge.set_valign(gtk::Align::End);
        badge.set_margin_end(8);
        badge.set_margin_bottom(8);
        badge.set_visible(false);

        let play = gtk::Image::from_icon_name("media-playback-start-symbolic");
        play.set_pixel_size(28);
        play.add_css_class("play-hint");
        play.set_halign(gtk::Align::Center);
        play.set_valign(gtk::Align::Center);
        play.set_can_target(false);

        let fav = gtk::Button::new();
        fav.add_css_class("card-action");
        fav.add_css_class("circular");
        fav.set_halign(gtk::Align::End);
        fav.set_valign(gtk::Align::Start);
        fav.set_margin_end(8);
        fav.set_margin_top(8);
        fav.set_tooltip_text(Some("Favorite"));
        set_fav_look(&fav, self.cfg.borrow().is_favorite(&path_str));
        {
            let weak = Rc::downgrade(self);
            let p = path_str.clone();
            fav.connect_clicked(move |_| {
                if let Some(l) = weak.upgrade() {
                    l.toggle_favorite(&p);
                }
            });
        }

        let more = gtk::MenuButton::new();
        more.set_icon_name("view-more-symbolic");
        more.add_css_class("card-action");
        more.add_css_class("circular");
        more.set_halign(gtk::Align::Start);
        more.set_valign(gtk::Align::Start);
        more.set_margin_start(8);
        more.set_margin_top(8);
        {
            let (cfg, clip) = (self.cfg.clone(), clip.clone());
            more.set_create_popup_func(move |btn| {
                let path = clip.path.to_string_lossy().into_owned();
                let fav = cfg.borrow().is_favorite(&path);
                btn.set_menu_model(Some(&build_menu(&path, fav, clip.frame.get().is_some_and(media::is_four_three))));
            });
        }

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&picture));
        overlay.add_overlay(&play);
        overlay.add_overlay(&badge);
        overlay.add_overlay(&fav);
        overlay.add_overlay(&more);
        overlay.set_overflow(gtk::Overflow::Hidden);

        let title = gtk::Label::new(Some(&clip.name));
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.add_css_class("heading");
        let meta = gtk::Label::new(Some(&format!(
            "{} · {}",
            media::relative_date(clip.mtime),
            media::human_size(clip.size)
        )));
        meta.set_xalign(0.0);
        meta.add_css_class("caption");
        meta.add_css_class("dim-label");

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_margin_start(12);
        text.set_margin_end(12);
        text.set_margin_top(10);
        text.set_margin_bottom(12);
        text.append(&title);
        text.append(&meta);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.add_css_class("clip-card");
        body.set_overflow(gtk::Overflow::Hidden);
        body.append(&overlay);
        body.append(&text);

        // Right-click opens the menu right at the cursor.
        let popover = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        popover.set_parent(&body);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        let rc = gtk::GestureClick::new();
        rc.set_button(gtk::gdk::BUTTON_SECONDARY);
        {
            let (cfg, clip, popover) = (self.cfg.clone(), clip.clone(), popover.clone());
            rc.connect_pressed(move |g, _, x, y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                let path = clip.path.to_string_lossy().into_owned();
                let fav = cfg.borrow().is_favorite(&path);
                popover.set_menu_model(Some(&build_menu(&path, fav, clip.frame.get().is_some_and(media::is_four_three))));
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            });
        }
        body.add_controller(rc);

        let child = gtk::FlowBoxChild::new();
        child.set_widget_name(&path_str);
        child.set_child(Some(&body));

        self.cards.borrow_mut().insert(
            path_str,
            Card {
                child: child.clone(),
                clip: clip.clone(),
                fav,
                popover,
            },
        );
        self.flow.append(&child);

        // Duration + thumbnail, off the main thread and bounded by the pool.
        let cached = self.durations.borrow().get(&clip.key).copied();
        let lib = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let p = clip.path.clone();
            let result = LIBRARY_POOL
                .run(move || {
                    let dur = cached.unwrap_or_else(|| media::probe_duration(&p));
                    (dur, media::ensure_thumbnail(&p, dur), media::frame_size(&p))
                })
                .await;
            let Some((dur, thumb, frame)) = result else { return };
            clip.frame.set(frame);
            if let Some(l) = lib.upgrade() {
                l.durations.borrow_mut().insert(clip.key.clone(), dur);
            }
            clip.duration.set(dur);
            if dur > 0.0 {
                badge.set_label(&media::human_duration(dur));
                badge.set_visible(true);
            }
            if let Some(t) = thumb {
                picture.set_filename(Some(&t));
            }
        });
    }
}

fn menu_item(label: &str, icon: &str, action: &str, path: &str) -> gio::MenuItem {
    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(Some(action), Some(&path.to_variant()));
    item.set_icon(&gio::ThemedIcon::new(icon));
    item
}

/// The clip context menu, grouped into sections. Built each time it opens so the
/// Favorite wording and the fullscreen option reflect the clip's current state.
fn build_menu(path: &str, is_fav: bool, four_three: bool) -> gio::Menu {
    let menu = gio::Menu::new();

    let main = gio::Menu::new();
    main.append_item(&menu_item("Edit & Trim", "media-playback-start-symbolic", "win.clip-open", path));
    main.append_item(&menu_item(
        if is_fav { "Remove from Favorites" } else { "Add to Favorites" },
        if is_fav { "starred-symbolic" } else { "non-starred-symbolic" },
        "win.clip-favorite",
        path,
    ));
    main.append_item(&menu_item("Copy File", "edit-copy-symbolic", "win.clip-copy", path));
    menu.append_section(None, &main);

    let manage = gio::Menu::new();
    if four_three {
        manage.append_item(&menu_item("Make Fullscreen (16:9)", "view-fullscreen-symbolic", "win.clip-fullscreen", path));
    }
    manage.append_item(&menu_item("Rename…", "document-edit-symbolic", "win.clip-rename", path));
    manage.append_item(&menu_item("Show in Folder", "folder-open-symbolic", "win.clip-folder", path));
    menu.append_section(None, &manage);

    let danger = gio::Menu::new();
    danger.append_item(&menu_item("Move to Trash", "user-trash-symbolic", "win.clip-delete", path));
    menu.append_section(None, &danger);
    menu
}

fn set_fav_look(btn: &gtk::Button, on: bool) {
    btn.set_icon_name(if on { "starred-symbolic" } else { "non-starred-symbolic" });
    if on {
        btn.add_css_class("on");
    } else {
        btn.remove_css_class("on");
    }
}
