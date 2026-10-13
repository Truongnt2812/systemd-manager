//! The main window: a service list on the left, status / journal / unit file on the right.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::journal::{self, JournalStream};
use crate::new_service;
use crate::systemd::{self, Action, Bus, Details, Res, Service};

const FILTERS: [&str; 5] = ["All", "Running", "Failed", "Inactive", "Enabled"];
const JOURNAL_LINES: [u32; 4] = [100, 500, 2000, 10000];
const JOURNAL_MAX_LINES: i32 = 20000;

/// Runs `work` on a worker thread and hands its result to `done` on the main thread.
pub fn background<T, W, D>(work: W, done: D)
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    D: FnOnce(T) + 'static,
{
    glib::spawn_future_local(async move {
        match gio::spawn_blocking(work).await {
            Ok(v) => done(v),
            Err(_) => eprintln!("background task panicked"),
        }
    });
}

struct StatusRows {
    loaded: adw::ActionRow,
    active: adw::ActionRow,
    file_state: adw::ActionRow,
    pid: adw::ActionRow,
    memory: adw::ActionRow,
    since: adw::ActionRow,
    path: adw::ActionRow,
}

struct ActionButtons {
    start: gtk::Button,
    stop: gtk::Button,
    restart: gtk::Button,
    reload: gtk::Button,
    enable: gtk::Button,
    disable: gtk::Button,
    mask: gtk::Button,
    unmask: gtk::Button,
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    filter: gtk::DropDown,
    count_label: gtk::Label,
    content_page: adw::NavigationPage,
    content_stack: gtk::Stack,
    name_label: gtk::Label,
    description_label: gtk::Label,
    rows: StatusRows,
    buttons: ActionButtons,
    journal_view: gtk::TextView,
    journal_follow: gtk::ToggleButton,
    journal_lines: gtk::DropDown,
    unit_view: gtk::TextView,
    unit_banner: adw::Banner,
    unit_path_label: gtk::Label,
    unit_save: gtk::Button,

    bus: Cell<Bus>,
    services: RefCell<Vec<Service>>,
    selected: RefCell<Option<String>>,
    details: RefCell<Option<Details>>,
    journal: RefCell<Option<JournalStream>>,
    journal_gen: Cell<u64>,
    list_gen: Cell<u64>,
    rebuilding: Cell<bool>,
    unit_loading: Cell<bool>,
    pending_select: RefCell<Option<String>>,
    refresh_pending: Cell<bool>,
}

fn property_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle("—")
        .use_markup(false)
        .subtitle_selectable(true)
        .build();
    row.add_css_class("property");
    row
}

fn icon_button(label: &str, icon: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder()
        .label(label)
        .icon_name(icon)
        .build();
    gtk::Button::builder().child(&content).build()
}

fn linked(buttons: &[&gtk::Button]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    b.add_css_class("linked");
    for btn in buttons {
        b.append(*btn);
    }
    b
}

fn state_class(active_state: &str) -> &'static str {
    match active_state {
        "active" => "success",
        "failed" => "error",
        "activating" | "deactivating" | "reloading" => "warning",
        _ => "dim-label",
    }
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(child)
        .vexpand(true)
        .hexpand(true)
        .build()
}

fn service_row(s: &Service) -> gtk::ListBoxRow {
    let name = gtk::Label::builder()
        .label(&s.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let subtitle = if !s.description.is_empty() && s.description != s.name {
        s.description.clone()
    } else if !s.file_state.is_empty() {
        format!("{} · {}", s.load_state, s.file_state)
    } else {
        s.load_state.clone()
    };
    let desc = gtk::Label::builder()
        .label(&subtitle)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label", "caption"])
        .build();
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&name);
    text.append(&desc);

    let state = gtk::Label::builder()
        .label(&s.active_state)
        .css_classes(["caption", state_class(&s.active_state)])
        .valign(gtk::Align::Center)
        .build();

    let hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(4)
        .margin_end(4)
        .build();
    hbox.append(&text);
    hbox.append(&state);

    let row = gtk::ListBoxRow::builder().child(&hbox).build();
    row.set_tooltip_text(Some(&format!(
        "{} — {} ({})",
        s.name, s.active_state, s.sub_state
    )));
    row
}

impl Window {
    pub fn build(app: &adw::Application) -> Rc<Self> {
        // ---- Sidebar -------------------------------------------------------
        let system_btn = gtk::ToggleButton::builder()
            .label("System")
            .active(true)
            .tooltip_text("System services (changes ask for your password)")
            .build();
        let user_btn = gtk::ToggleButton::builder()
            .label("User")
            .group(&system_btn)
            .tooltip_text("Your own user services")
            .build();
        let bus_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bus_box.add_css_class("linked");
        bus_box.append(&system_btn);
        bus_box.append(&user_btn);

        let refresh_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh")
            .build();
        let new_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New service")
            .build();
        let side_header = adw::HeaderBar::new();
        side_header.set_title_widget(Some(&bus_box));
        side_header.pack_start(&refresh_btn);
        side_header.pack_end(&new_btn);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search services")
            .hexpand(true)
            .build();
        let filter = gtk::DropDown::from_strings(&FILTERS);
        filter.set_tooltip_text(Some("Filter by state"));
        let filter_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        filter_box.append(&search);
        filter_box.append(&filter);

        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);
        let list_scroll = scrolled(&list);
        list_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);

        let count_label = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .margin_top(6)
            .margin_bottom(6)
            .build();

        let side_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        side_box.append(&filter_box);
        side_box.append(&list_scroll);
        side_box.append(&count_label);
        let side_tv = adw::ToolbarView::new();
        side_tv.add_top_bar(&side_header);
        side_tv.set_content(Some(&side_box));
        let sidebar_page = adw::NavigationPage::new(&side_tv, "Services");

        // ---- Status tab ----------------------------------------------------
        let name_label = gtk::Label::builder()
            .css_classes(["title-2"])
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        let description_label = gtk::Label::builder()
            .css_classes(["dim-label"])
            .xalign(0.0)
            .wrap(true)
            .build();

        let buttons = ActionButtons {
            start: icon_button("Start", "media-playback-start-symbolic"),
            stop: icon_button("Stop", "media-playback-stop-symbolic"),
            restart: icon_button("Restart", "view-refresh-symbolic"),
            reload: icon_button("Reload", "document-revert-symbolic"),
            enable: icon_button("Enable", "object-select-symbolic"),
            disable: icon_button("Disable", "window-close-symbolic"),
            mask: icon_button("Mask", "action-unavailable-symbolic"),
            unmask: icon_button("Unmask", "edit-undo-symbolic"),
        };
        buttons
            .restart
            .set_tooltip_text(Some("Stop and start the service"));
        buttons
            .reload
            .set_tooltip_text(Some("Ask the service to reload its configuration"));
        buttons
            .enable
            .set_tooltip_text(Some("Start automatically at boot / login"));
        buttons
            .mask
            .set_tooltip_text(Some("Prevent the service from being started at all"));
        let run_box = linked(&[
            &buttons.start,
            &buttons.stop,
            &buttons.restart,
            &buttons.reload,
        ]);
        run_box.set_margin_top(12);
        let boot_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        boot_box.append(&linked(&[&buttons.enable, &buttons.disable]));
        boot_box.append(&buttons.mask);
        boot_box.append(&buttons.unmask);
        run_box.set_halign(gtk::Align::Start);
        boot_box.set_halign(gtk::Align::Start);

        let head_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        head_box.append(&name_label);
        head_box.append(&description_label);
        head_box.append(&run_box);
        head_box.append(&boot_box);
        let head_group = adw::PreferencesGroup::new();
        head_group.add(&head_box);

        let rows = StatusRows {
            loaded: property_row("Loaded"),
            active: property_row("Active"),
            file_state: property_row("Unit file state"),
            pid: property_row("Main PID"),
            memory: property_row("Memory"),
            since: property_row("Active since"),
            path: property_row("Unit file"),
        };
        let status_group = adw::PreferencesGroup::new();
        for r in [
            &rows.active,
            &rows.file_state,
            &rows.loaded,
            &rows.since,
            &rows.pid,
            &rows.memory,
            &rows.path,
        ] {
            status_group.add(r);
        }
        let status_page = adw::PreferencesPage::new();
        status_page.add(&head_group);
        status_page.add(&status_group);

        // ---- Journal tab ---------------------------------------------------
        let journal_view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        let journal_lines = gtk::DropDown::from_strings(&[
            "Last 100 lines",
            "Last 500 lines",
            "Last 2000 lines",
            "Last 10000 lines",
        ]);
        journal_lines.set_selected(1);
        let journal_follow = gtk::ToggleButton::builder()
            .child(
                &adw::ButtonContent::builder()
                    .label("Follow")
                    .icon_name("media-playback-start-symbolic")
                    .build(),
            )
            .tooltip_text("Keep showing new log lines as they arrive")
            .build();
        let journal_reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload log")
            .build();
        let journal_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        journal_bar.append(&journal_lines);
        journal_bar.append(&journal_follow);
        journal_bar.append(&spacer);
        journal_bar.append(&journal_reload);
        let journal_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        journal_box.append(&journal_bar);
        journal_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        journal_box.append(&scrolled(&journal_view));

        // ---- Unit file tab -------------------------------------------------
        let unit_view = gtk::TextView::builder()
            .monospace(true)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        let unit_banner = adw::Banner::new("");
        let unit_path_label = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .hexpand(true)
            .xalign(0.0)
            .selectable(true)
            .build();
        let unit_save = gtk::Button::builder()
            .label("Save and Reload")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .tooltip_text("Write the unit file and run daemon-reload")
            .build();
        let unit_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        unit_bar.append(&unit_path_label);
        unit_bar.append(&unit_save);
        let unit_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        unit_box.append(&unit_banner);
        unit_box.append(&unit_bar);
        unit_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        unit_box.append(&scrolled(&unit_view));

        // ---- Content pane --------------------------------------------------
        let view_stack = adw::ViewStack::new();
        view_stack.add_titled_with_icon(
            &status_page,
            Some("status"),
            "Status",
            "dialog-information-symbolic",
        );
        view_stack.add_titled_with_icon(
            &journal_box,
            Some("journal"),
            "Journal",
            "utilities-terminal-symbolic",
        );
        view_stack.add_titled_with_icon(
            &unit_box,
            Some("unit"),
            "Unit File",
            "document-edit-symbolic",
        );
        let switcher = adw::ViewSwitcher::builder()
            .stack(&view_stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        let details_header = adw::HeaderBar::new();
        details_header.set_title_widget(Some(&switcher));
        let details_tv = adw::ToolbarView::new();
        details_tv.add_top_bar(&details_header);
        details_tv.set_content(Some(&view_stack));

        let empty_tv = adw::ToolbarView::new();
        empty_tv.add_top_bar(&adw::HeaderBar::new());
        empty_tv.set_content(Some(
            &adw::StatusPage::builder()
                .icon_name("emblem-system-symbolic")
                .title("No Service Selected")
                .description("Pick a service from the list to see its status, logs and unit file.")
                .build(),
        ));

        let content_stack = gtk::Stack::new();
        content_stack.add_named(&empty_tv, Some("empty"));
        content_stack.add_named(&details_tv, Some("details"));
        let content_page = adw::NavigationPage::new(&content_stack, "Service");

        // ---- Window --------------------------------------------------------
        let split = adw::NavigationSplitView::new();
        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&content_page));
        split.set_min_sidebar_width(300.0);
        split.set_sidebar_width_fraction(0.3);
        split.set_max_sidebar_width(420.0);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Systemd Manager")
            .default_width(1100)
            .default_height(720)
            .width_request(360)
            .height_request(400)
            .content(&toasts)
            .build();
        let bp = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 720sp").expect("valid condition"),
        );
        bp.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(bp);

        let w = Rc::new(Window {
            window,
            toasts,
            split,
            list,
            search,
            filter,
            count_label,
            content_page,
            content_stack,
            name_label,
            description_label,
            rows,
            buttons,
            journal_view,
            journal_follow,
            journal_lines,
            unit_view,
            unit_banner,
            unit_path_label,
            unit_save,
            bus: Cell::new(Bus::System),
            services: RefCell::new(Vec::new()),
            selected: RefCell::new(None),
            details: RefCell::new(None),
            journal: RefCell::new(None),
            journal_gen: Cell::new(0),
            list_gen: Cell::new(0),
            rebuilding: Cell::new(false),
            unit_loading: Cell::new(false),
            pending_select: RefCell::new(None),
            refresh_pending: Cell::new(false),
        });

        // ---- Signals -------------------------------------------------------
        let weak = Rc::downgrade(&w);
        system_btn.connect_toggled(move |btn| {
            if let Some(w) = weak.upgrade() {
                w.set_bus(if btn.is_active() {
                    Bus::System
                } else {
                    Bus::User
                });
            }
        });
        let weak = Rc::downgrade(&w);
        refresh_btn.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.refresh_list();
                w.load_details(false);
            }
        });
        let weak = Rc::downgrade(&w);
        new_btn.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                new_service::show(&w);
            }
        });

        let weak = Rc::downgrade(&w);
        w.list
            .set_filter_func(move |row| weak.upgrade().is_none_or(|w| w.row_visible(row.index())));
        let weak = Rc::downgrade(&w);
        w.search.connect_search_changed(move |_| {
            if let Some(w) = weak.upgrade() {
                w.refilter();
            }
        });
        let weak = Rc::downgrade(&w);
        w.filter.connect_selected_notify(move |_| {
            if let Some(w) = weak.upgrade() {
                w.refilter();
            }
        });
        let weak = Rc::downgrade(&w);
        w.list.connect_row_selected(move |_, row| {
            let (Some(w), Some(row)) = (weak.upgrade(), row) else {
                return;
            };
            if w.rebuilding.get() {
                return;
            }
            let name = w
                .services
                .borrow()
                .get(row.index() as usize)
                .map(|s| s.name.clone());
            if let Some(name) = name {
                w.select(name);
            }
        });
        let weak = Rc::downgrade(&w);
        w.list.connect_row_activated(move |_, _| {
            if let Some(w) = weak.upgrade() {
                w.split.set_show_content(true);
            }
        });

        let b = &w.buttons;
        for (btn, action) in [
            (&b.start, Action::Start),
            (&b.stop, Action::Stop),
            (&b.restart, Action::Restart),
            (&b.reload, Action::Reload),
            (&b.enable, Action::Enable),
            (&b.disable, Action::Disable),
            (&b.mask, Action::Mask),
            (&b.unmask, Action::Unmask),
        ] {
            let weak = Rc::downgrade(&w);
            btn.connect_clicked(move |_| {
                if let Some(w) = weak.upgrade() {
                    w.run(action);
                }
            });
        }

        let weak = Rc::downgrade(&w);
        w.journal_follow.connect_toggled(move |_| {
            if let Some(w) = weak.upgrade() {
                w.restart_journal();
            }
        });
        let weak = Rc::downgrade(&w);
        w.journal_lines.connect_selected_notify(move |_| {
            if let Some(w) = weak.upgrade() {
                w.restart_journal();
            }
        });
        let weak = Rc::downgrade(&w);
        journal_reload.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.restart_journal();
            }
        });

        let weak = Rc::downgrade(&w);
        w.unit_view.buffer().connect_changed(move |_| {
            if let Some(w) = weak.upgrade() {
                if !w.unit_loading.get() {
                    w.unit_save.set_sensitive(true);
                }
            }
        });
        let weak = Rc::downgrade(&w);
        w.unit_save.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.save_unit();
            }
        });

        // The window owns the only strong reference to our state.
        let strong = w.clone();
        w.window.connect_close_request(move |_| {
            strong.journal.replace(None);
            glib::Propagation::Proceed
        });

        w.refresh_list();
        w.watch_systemd();
        w
    }

    pub fn bus(&self) -> Bus {
        self.bus.get()
    }

    pub fn toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        toast.set_use_markup(false);
        toast.set_timeout(5);
        self.toasts.add_toast(toast);
    }

    fn set_bus(self: &Rc<Self>, bus: Bus) {
        if self.bus.get() == bus {
            return;
        }
        self.bus.set(bus);
        self.selected.replace(None);
        self.details.replace(None);
        self.journal.replace(None);
        self.content_stack.set_visible_child_name("empty");
        self.content_page.set_title("Service");
        self.split.set_show_content(false);
        self.services.borrow_mut().clear();
        self.list.remove_all();
        self.update_count();
        self.refresh_list();
    }

    // ---- Service list ------------------------------------------------------

    pub fn refresh_list(self: &Rc<Self>) {
        let bus = self.bus.get();
        let generation = self.list_gen.get() + 1;
        self.list_gen.set(generation);
        let weak = Rc::downgrade(self);
        background(
            move || systemd::list_services(bus),
            move |res| {
                let Some(w) = weak.upgrade() else { return };
                if w.list_gen.get() != generation || w.bus.get() != bus {
                    return;
                }
                match res {
                    Ok(services) => w.set_services(services),
                    Err(e) => w.toast(&format!("Could not list services: {e}")),
                }
            },
        );
    }

    /// Refreshes the list and the selected service whenever systemd reports a change,
    /// including ones made outside this app (systemctl, crashes, package updates).
    fn watch_systemd(self: &Rc<Self>) {
        for bus in [Bus::System, Bus::User] {
            let (tx, rx) = async_channel::bounded::<()>(1);
            std::thread::spawn(move || {
                if let Err(e) = systemd::watch(bus, || {
                    let _ = tx.try_send(());
                }) {
                    eprintln!("not watching {bus:?} services for changes: {e}");
                }
            });
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                while rx.recv().await.is_ok() {
                    let Some(w) = weak.upgrade() else { break };
                    if w.bus.get() == bus {
                        w.schedule_refresh();
                    }
                }
            });
        }
    }

    /// Coalesces bursts of change signals into one refresh.
    fn schedule_refresh(self: &Rc<Self>) {
        if self.refresh_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(w) = weak.upgrade() {
                w.refresh_pending.set(false);
                w.refresh_list();
                w.load_details(false);
            }
        });
    }

    /// Selects `name` once the next list refresh finishes.
    pub fn select_after_refresh(self: &Rc<Self>, name: String) {
        self.pending_select.replace(Some(name));
        self.refresh_list();
    }

    fn set_services(self: &Rc<Self>, services: Vec<Service>) {
        self.rebuilding.set(true);
        let rows: Vec<_> = services.iter().map(service_row).collect();
        self.services.replace(services);
        self.list.remove_all();
        for row in &rows {
            self.list.append(row);
        }
        let pending = self.pending_select.take();
        let wanted = pending.clone().or_else(|| self.selected.borrow().clone());
        if let Some(name) = &wanted {
            let idx = self.services.borrow().iter().position(|s| &s.name == name);
            if let Some(row) = idx.and_then(|i| self.list.row_at_index(i as i32)) {
                self.list.select_row(Some(&row));
                if pending.is_some() {
                    row.grab_focus();
                }
            }
        }
        self.rebuilding.set(false);
        self.update_count();
        if let Some(name) = pending {
            self.select(name);
            self.split.set_show_content(true);
        }
    }

    fn row_visible(&self, index: i32) -> bool {
        let services = self.services.borrow();
        let Some(s) = services.get(index as usize) else {
            return true;
        };
        self.matches(s)
    }

    fn matches(&self, s: &Service) -> bool {
        let query = self.search.text().to_lowercase();
        if !query.is_empty()
            && !s.name.to_lowercase().contains(&query)
            && !s.description.to_lowercase().contains(&query)
        {
            return false;
        }
        match self.filter.selected() {
            1 => s.active_state == "active",
            2 => s.active_state == "failed",
            3 => s.active_state == "inactive",
            4 => s.file_state.starts_with("enabled"),
            _ => true,
        }
    }

    fn refilter(&self) {
        self.list.invalidate_filter();
        self.update_count();
    }

    fn update_count(&self) {
        let services = self.services.borrow();
        let shown = services.iter().filter(|s| self.matches(s)).count();
        let failed = services
            .iter()
            .filter(|s| s.active_state == "failed")
            .count();
        let mut text = format!("{shown} of {} services", services.len());
        if failed > 0 {
            text += &format!(" · {failed} failed");
        }
        self.count_label.set_label(&text);
    }

    // ---- Details -----------------------------------------------------------

    fn select(self: &Rc<Self>, name: String) {
        if self.selected.borrow().as_deref() == Some(name.as_str()) {
            return;
        }
        self.selected.replace(Some(name.clone()));
        self.details.replace(None);
        self.content_stack.set_visible_child_name("details");
        self.content_page.set_title(&name);
        self.name_label.set_label(&name);
        self.description_label.set_label("");
        self.load_details(true);
        self.restart_journal();
    }

    fn load_details(self: &Rc<Self>, reload_file: bool) {
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let bus = self.bus.get();
        let weak = Rc::downgrade(self);
        let query = name.clone();
        background(
            move || systemd::details(bus, &query),
            move |res| {
                let Some(w) = weak.upgrade() else { return };
                if w.bus.get() != bus || w.selected.borrow().as_deref() != Some(name.as_str()) {
                    return;
                }
                match res {
                    Ok(d) => {
                        w.show_details(&d);
                        if reload_file {
                            w.load_unit_file(&name, &d);
                        }
                        w.details.replace(Some(d));
                    }
                    Err(e) => w.toast(&format!("Could not load {name}: {e}")),
                }
            },
        );
    }

    fn show_details(&self, d: &Details) {
        let dash = |s: &str| {
            if s.is_empty() {
                "—".to_string()
            } else {
                s.to_string()
            }
        };
        let name = self.selected.borrow().clone().unwrap_or_default();
        self.description_label.set_label(&d.description);
        self.description_label
            .set_visible(!d.description.is_empty() && d.description != name);
        self.rows.loaded.set_subtitle(&dash(&d.load_state));
        let active = if d.sub_state.is_empty() || d.sub_state == d.active_state {
            d.active_state.clone()
        } else {
            format!("{} ({})", d.active_state, d.sub_state)
        };
        self.rows.active.set_subtitle(&active);
        for class in ["success", "error", "warning", "dim-label"] {
            self.rows.active.remove_css_class(class);
        }
        self.rows.active.add_css_class(state_class(&d.active_state));
        self.rows.file_state.set_subtitle(&dash(&d.file_state));
        self.rows.pid.set_subtitle(&if d.main_pid == 0 {
            "—".to_string()
        } else {
            d.main_pid.to_string()
        });
        self.rows.memory.set_subtitle(
            &d.memory
                .map(|m| glib::format_size(m).to_string())
                .unwrap_or_else(|| "—".into()),
        );
        let since = (d.active_since > 0 && d.active_state == "active")
            .then(|| glib::DateTime::from_unix_local((d.active_since / 1_000_000) as i64).ok())
            .flatten()
            .and_then(|t| t.format("%Y-%m-%d %H:%M:%S").ok())
            .map(|s| s.to_string());
        self.rows
            .since
            .set_subtitle(&since.unwrap_or_else(|| "—".into()));
        self.rows.path.set_subtitle(&dash(&d.fragment_path));

        let b = &self.buttons;
        let running = matches!(d.active_state.as_str(), "active" | "reloading");
        let busy = matches!(d.active_state.as_str(), "activating" | "deactivating");
        let masked = d.file_state.starts_with("masked") || d.load_state == "masked";
        let enabled = d.file_state.starts_with("enabled");
        b.start.set_sensitive(!running && !masked);
        b.stop.set_sensitive(running || busy);
        b.restart.set_sensitive(!masked);
        b.reload.set_sensitive(running);
        b.enable.set_sensitive(
            !masked
                && !enabled
                && !matches!(
                    d.file_state.as_str(),
                    "" | "static" | "generated" | "transient"
                ),
        );
        b.disable.set_sensitive(enabled || d.file_state == "linked");
        b.mask.set_visible(!masked);
        b.unmask.set_visible(masked);
    }

    fn run(self: &Rc<Self>, action: Action) {
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let bus = self.bus.get();
        let weak = Rc::downgrade(self);
        background(
            move || systemd::run_action(bus, &name, action),
            move |res| {
                let Some(w) = weak.upgrade() else { return };
                match res {
                    Ok(msg) => w.toast(&msg),
                    Err(e) => w.toast(&e),
                }
                // Jobs run asynchronously in systemd; give them a moment to settle.
                let weak = Rc::downgrade(&w);
                glib::timeout_add_local_once(Duration::from_millis(700), move || {
                    if let Some(w) = weak.upgrade() {
                        w.load_details(matches!(action, Action::Mask | Action::Unmask));
                        w.refresh_list();
                    }
                });
            },
        );
    }

    // ---- Journal -----------------------------------------------------------

    fn restart_journal(self: &Rc<Self>) {
        self.journal.replace(None);
        let generation = self.journal_gen.get() + 1;
        self.journal_gen.set(generation);
        let buffer = self.journal_view.buffer();
        buffer.set_text("");
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let lines = JOURNAL_LINES
            .get(self.journal_lines.selected() as usize)
            .copied()
            .unwrap_or(500);
        let follow = self.journal_follow.is_active();
        match journal::spawn(self.bus.get(), &name, lines, follow) {
            Err(e) => buffer.set_text(&format!("Could not run journalctl: {e}")),
            Ok((stream, rx)) => {
                self.journal.replace(Some(stream));
                let weak = Rc::downgrade(self);
                glib::spawn_future_local(async move {
                    while let Ok(first) = rx.recv().await {
                        let Some(w) = weak.upgrade() else { break };
                        if w.journal_gen.get() != generation {
                            break;
                        }
                        let mut chunk = first;
                        while chunk.len() < 256 * 1024 {
                            match rx.try_recv() {
                                Ok(more) => chunk.push_str(&more),
                                Err(_) => break,
                            }
                        }
                        w.append_journal(&chunk);
                    }
                });
            }
        }
    }

    fn append_journal(&self, text: &str) {
        let buffer = self.journal_view.buffer();
        buffer.insert(&mut buffer.end_iter(), text);
        let excess = buffer.line_count() - JOURNAL_MAX_LINES;
        if excess > 0 {
            if let Some(mut cut) = buffer.iter_at_line(excess) {
                buffer.delete(&mut buffer.start_iter(), &mut cut);
            }
        }
        let mark = buffer
            .mark("end")
            .unwrap_or_else(|| buffer.create_mark(Some("end"), &buffer.end_iter(), false));
        buffer.move_mark(&mark, &buffer.end_iter());
        self.journal_view.scroll_mark_onscreen(&mark);
    }

    // ---- Unit file ---------------------------------------------------------

    fn load_unit_file(&self, name: &str, d: &Details) {
        let buffer = self.unit_view.buffer();
        let fragment = d.fragment_path.as_str();
        self.unit_loading.set(true);
        let mut editable = true;
        if fragment == "/dev/null" || d.load_state == "masked" {
            buffer.set_text("# This service is masked. Unmask it to view or edit its unit file.\n");
            editable = false;
        } else if fragment.is_empty() {
            buffer.set_text("# This unit has no unit file on disk.\n");
            editable = false;
        } else {
            match systemd::read_file(fragment) {
                Ok(text) => buffer.set_text(&text),
                Err(e) => {
                    buffer.set_text(&format!("# {e}\n"));
                    editable = false;
                }
            }
        }
        self.unit_loading.set(false);
        self.unit_view.set_editable(editable);
        self.unit_save.set_sensitive(false);
        self.unit_path_label
            .set_label(if fragment.is_empty() { "—" } else { fragment });

        let target = systemd::editable_path(self.bus.get(), name, fragment);
        if editable && target != Path::new(fragment) {
            self.unit_banner.set_title(&format!(
                "This is a vendor unit file. Saving writes an override copy to {}",
                target.display()
            ));
            self.unit_banner.set_revealed(true);
        } else {
            self.unit_banner.set_revealed(false);
        }
    }

    fn save_unit(self: &Rc<Self>) {
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let fragment = self
            .details
            .borrow()
            .as_ref()
            .map(|d| d.fragment_path.clone())
            .unwrap_or_default();
        let bus = self.bus.get();
        let target = systemd::editable_path(bus, &name, &fragment);
        let buffer = self.unit_view.buffer();
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        self.unit_save.set_sensitive(false);
        let weak = Rc::downgrade(self);
        background(
            move || -> Res<PathBuf> {
                systemd::write_unit_file(bus, &target, &text)?;
                systemd::daemon_reload(bus)?;
                Ok(target)
            },
            move |res| {
                let Some(w) = weak.upgrade() else { return };
                match res {
                    Ok(path) => {
                        w.toast(&format!("Saved {} and reloaded systemd", path.display()));
                        w.load_details(true);
                    }
                    Err(e) => {
                        w.unit_save.set_sensitive(true);
                        w.toast(&format!("Could not save: {e}"));
                    }
                }
            },
        );
    }
}
