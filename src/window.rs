//! The main window: a service list on the left, status / journal / unit file on the right.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use sourceview5::prelude::*;

use crate::journal::{self, JournalStream};
use crate::new_service;
use crate::resources::{self, ResourcesView};
use crate::systemd::{self, Action, Bus, Details, EditTarget, Res, Service};
use crate::timers;

const SHORTCUTS_UI: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <object class="GtkShortcutsWindow" id="help_overlay">
    <property name="modal">True</property>
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">shortcuts</property>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">General</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Search services, or the log on the Journal tab</property><property name="accelerator">&lt;Ctrl&gt;f</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">New service</property><property name="accelerator">&lt;Ctrl&gt;n</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Timers</property><property name="accelerator">&lt;Ctrl&gt;t</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Reload (the list updates by itself)</property><property name="accelerator">F5</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">System services</property><property name="accelerator">&lt;Ctrl&gt;1</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">User services</property><property name="accelerator">&lt;Ctrl&gt;2</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Keyboard shortcuts</property><property name="accelerator">&lt;Ctrl&gt;question</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Close window</property><property name="accelerator">&lt;Ctrl&gt;w</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Selected Service</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Start</property><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;s</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Stop</property><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;x</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Restart</property><property name="accelerator">&lt;Ctrl&gt;r</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Save unit file and reload</property><property name="accelerator">&lt;Ctrl&gt;s</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Pause or resume the journal</property><property name="accelerator">&lt;Ctrl&gt;p</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Status, Resources, Journal and Unit File tabs</property><property name="accelerator">&lt;Alt&gt;1 &lt;Alt&gt;2 &lt;Alt&gt;3 &lt;Alt&gt;4</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Next / previous match in the log</property><property name="accelerator">Return &lt;Shift&gt;Return</property></object></child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>
"#;

const FILTERS: [&str; 5] = ["All", "Running", "Failed", "Inactive", "Enabled"];
const JOURNAL_LINES: [u32; 4] = [100, 500, 2000, 10000];
const JOURNAL_MAX_LINES: i32 = 20000;
const JOURNAL_PRIORITIES: [(&str, Option<&str>); 3] = [
    ("All messages", None),
    ("Warnings and errors", Some("warning")),
    ("Errors only", Some("err")),
];
const JOURNAL_BOOTS: [&str; 2] = ["All boots", "This boot"];
const JOURNAL_SINCE: [(&str, Option<&str>); 5] = [
    ("Any time", None),
    ("Last 15 minutes", Some("-15min")),
    ("Last hour", Some("-1h")),
    ("Last 24 hours", Some("-1d")),
    ("Last 7 days", Some("-7d")),
];

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
    result: adw::ActionRow,
    last_exit: adw::ActionRow,
    cpu: adw::ActionRow,
    tasks: adw::ActionRow,
    restarts: adw::ActionRow,
    triggered_by: adw::ActionRow,
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
    reset_failed: gtk::Button,
    delete: gtk::Button,
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    system_btn: gtk::ToggleButton,
    user_btn: gtk::ToggleButton,
    toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    list: gtk::ListView,
    /// Every service of the current bus, as `BoxedAnyObject`s holding a `Service`.
    store: gio::ListStore,
    list_filter: gtk::CustomFilter,
    selection: gtk::SingleSelection,
    search: gtk::SearchEntry,
    filter: gtk::DropDown,
    count_label: gtk::Label,
    content_page: adw::NavigationPage,
    content_stack: gtk::Stack,
    view_stack: adw::ViewStack,
    resources: Rc<ResourcesView>,
    /// A usage sample is being fetched.
    sampling: Cell<bool>,
    name_label: gtk::Label,
    description_label: gtk::Label,
    rows: StatusRows,
    buttons: ActionButtons,
    journal_view: gtk::TextView,
    journal_pause: gtk::ToggleButton,
    journal_search: gtk::SearchEntry,
    journal_matches_label: gtk::Label,
    journal_lines: gtk::DropDown,
    journal_filter_btn: gtk::MenuButton,
    journal_priority: gtk::DropDown,
    journal_boot: gtk::DropDown,
    journal_since: gtk::DropDown,
    unit_view: sourceview5::View,
    unit_banner: adw::Banner,
    unit_path_label: gtk::Label,
    unit_save: gtk::Button,
    unit_revert: gtk::Button,
    unit_original_btn: gtk::ToggleButton,
    unit_original_view: sourceview5::View,
    unit_stack: gtk::Stack,

    bus: Cell<Bus>,
    services: RefCell<Vec<Service>>,
    selected: RefCell<Option<String>>,
    details: RefCell<Option<Details>>,
    journal: RefCell<Option<JournalStream>>,
    journal_gen: Cell<u64>,
    /// Lines that arrived while the journal was paused.
    journal_held: RefCell<String>,
    journal_matches: Cell<usize>,
    list_gen: Cell<u64>,
    rebuilding: Cell<bool>,
    unit_loading: Cell<bool>,
    /// The unit file editor has edits that haven't been saved.
    unit_dirty: Cell<bool>,
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

/// A source view for unit files: INI highlighting (unit files use that syntax),
/// line numbers, and a color scheme that follows the light/dark style.
fn unit_file_view(editable: bool) -> sourceview5::View {
    let buffer = sourceview5::Buffer::new(None);
    if let Some(lang) = sourceview5::LanguageManager::default().language("ini") {
        buffer.set_language(Some(&lang));
    }
    buffer.set_highlight_matching_brackets(false);
    let apply_scheme = {
        let buffer = buffer.clone();
        move |style: &adw::StyleManager| {
            let name = if style.is_dark() {
                "Adwaita-dark"
            } else {
                "Adwaita"
            };
            if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme(name) {
                buffer.set_style_scheme(Some(&scheme));
            }
        }
    };
    let style = adw::StyleManager::default();
    apply_scheme(&style);
    style.connect_dark_notify(apply_scheme);

    sourceview5::View::builder()
        .buffer(&buffer)
        .editable(editable)
        .monospace(true)
        .show_line_numbers(true)
        .highlight_current_line(editable)
        .tab_width(4)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build()
}

fn service_subtitle(s: &Service) -> String {
    if !s.description.is_empty() && s.description != s.name {
        s.description.clone()
    } else if !s.file_state.is_empty() {
        format!("{} · {}", s.load_state, s.file_state)
    } else {
        s.load_state.clone()
    }
}

/// changes as possible: unchanged rows are left alone and only changed services
/// are replaced, so the scroll position survives a refresh.
/// in place, so the selection and scroll position survive a refresh.
fn merge_services(store: &gio::ListStore, services: &[Service]) {
    let item = |pos: u32| store.item(pos).and_downcast::<glib::BoxedAnyObject>();
    let mut pos = 0u32;
    for s in services {
        loop {
            let Some(obj) = item(pos) else {
                store.append(&glib::BoxedAnyObject::new(s.clone()));
                pos += 1;
                break;
            };
            let order = obj.borrow::<Service>().sort_key().cmp(&s.sort_key());
            match order {
                // The service at `pos` is gone.
                std::cmp::Ordering::Less => store.remove(pos),
                std::cmp::Ordering::Equal => {
                    let changed = *obj.borrow::<Service>() != *s;
                    if changed {
                        // A new object: the list view keeps a row as it is when it
                        // gets the same object back. The caller restores the selection.
                        store.splice(pos, 1, &[glib::BoxedAnyObject::new(s.clone())]);
                    }
                    pos += 1;
                    break;
                }
                std::cmp::Ordering::Greater => {
                    store.insert(pos, &glib::BoxedAnyObject::new(s.clone()));
                    pos += 1;
                    break;
                }
            }
        }
    }
    let extra = store.n_items().saturating_sub(pos);
    if extra > 0 {
        store.splice(pos, extra, &[] as &[glib::Object]);
    }
}

/// Builds the widgets of a service row once; `bind_service_row` fills them in.
/// A click on a row also opens its details when the window is narrow.
fn service_row_factory(split: &adw::NavigationSplitView) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let split = split.downgrade();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let desc = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.append(&name);
        text.append(&desc);
        let state = gtk::Label::builder()
            .css_classes(["caption"])
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
        let click = gtk::GestureClick::new();
        let split = split.clone();
        click.connect_released(move |_, _, _, _| {
            if let Some(split) = split.upgrade() {
                split.set_show_content(true);
            }
        });
        hbox.add_controller(click);
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(obj), Some(hbox)) = (
            item.item().and_downcast::<glib::BoxedAnyObject>(),
            item.child(),
        ) else {
            return;
        };
        let s = obj.borrow::<Service>();
        let text = hbox.first_child();
        let name = text.as_ref().and_then(|t| t.first_child());
        let desc = name.as_ref().and_then(|n| n.next_sibling());
        let state = hbox.last_child();
        for (widget, label) in [
            (name, s.name.clone()),
            (desc, service_subtitle(&s)),
            (state.clone(), s.active_state.clone()),
        ] {
            if let Some(l) = widget.and_downcast::<gtk::Label>() {
                l.set_label(&label);
            }
        }
        if let Some(state) = state {
            state.set_css_classes(&["caption", state_class(&s.active_state)]);
        }
        hbox.set_tooltip_text(Some(&format!(
            "{} — {} ({})",
            s.name, s.active_state, s.sub_state
        )));
    });
    factory
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

        let new_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New service")
            .build();
        let timers_btn = gtk::Button::builder()
            .icon_name("alarm-symbolic")
            .tooltip_text("Timers")
            .build();
        let side_header = adw::HeaderBar::new();
        side_header.set_title_widget(Some(&bus_box));
        side_header.pack_start(&timers_btn);
        let main_menu = gio::Menu::new();
        main_menu.append(Some("_Keyboard Shortcuts"), Some("win.show-help-overlay"));
        main_menu.append(Some("_About Systemd Manager"), Some("win.about"));
        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main menu")
            .menu_model(&main_menu)
            .primary(true)
            .build();
        side_header.pack_end(&menu_btn);
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

        // Rows are created only for visible services and only changed ones are replaced, so a
        // refresh doesn't rebuild hundreds of widgets or lose the scroll position.
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let list_filter = gtk::CustomFilter::new(|_| true);
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(list_filter.clone()));
        let selection = gtk::SingleSelection::builder()
            .model(&filtered)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let list = gtk::ListView::builder()
            .model(&selection)
            .css_classes(["navigation-sidebar"])
            .build();
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
            reset_failed: icon_button("Reset Failed", "edit-clear-symbolic"),
            delete: icon_button("Delete", "user-trash-symbolic"),
        };
        buttons.reset_failed.set_tooltip_text(Some(
            "Clear the failed state and the restart counter, so the service can be started again",
        ));
        buttons.delete.add_css_class("destructive-action");
        buttons.delete.set_visible(false);
        buttons.delete.set_tooltip_text(Some(
            "Stop and disable the service and delete its unit file",
        ));
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
        boot_box.append(&buttons.reset_failed);
        boot_box.append(&buttons.delete);
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
            result: property_row("Last result"),
            last_exit: property_row("Main process exit"),
            cpu: property_row("CPU time"),
            tasks: property_row("Tasks"),
            restarts: property_row("Automatic restarts"),
            triggered_by: property_row("Started by"),
        };
        let status_group = adw::PreferencesGroup::new();
        for r in [
            &rows.active,
            &rows.file_state,
            &rows.result,
            &rows.last_exit,
            &rows.loaded,
            &rows.triggered_by,
            &rows.since,
            &rows.pid,
            &rows.memory,
            &rows.cpu,
            &rows.tasks,
            &rows.restarts,
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
        journal_view.buffer().create_tag(
            Some("match"),
            &[(
                "background-rgba",
                &gtk::gdk::RGBA::new(0.96, 0.76, 0.07, 0.45).to_value(),
            )],
        );
        journal_view.buffer().create_tag(
            Some("current-match"),
            &[(
                "background-rgba",
                &gtk::gdk::RGBA::new(1.0, 0.47, 0.0, 0.8).to_value(),
            )],
        );
        let journal_pause = gtk::ToggleButton::builder()
            .child(
                &adw::ButtonContent::builder()
                    .label("Pause")
                    .icon_name("media-playback-pause-symbolic")
                    .build(),
            )
            .tooltip_text("Pause the live log; new lines are kept until you resume")
            .build();
        let journal_search = gtk::SearchEntry::builder()
            .placeholder_text("Find in log")
            .hexpand(true)
            .build();
        let journal_matches_label = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .build();
        let journal_prev = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Previous match (Shift+Enter)")
            .build();
        let journal_next = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("Next match (Enter)")
            .build();
        let journal_reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload log")
            .build();
        let journal_save = gtk::Button::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text("Save log to a file")
            .build();

        let journal_priority = gtk::DropDown::from_strings(
            &JOURNAL_PRIORITIES.iter().map(|p| p.0).collect::<Vec<_>>(),
        );
        let journal_boot = gtk::DropDown::from_strings(&JOURNAL_BOOTS);
        let journal_since =
            gtk::DropDown::from_strings(&JOURNAL_SINCE.iter().map(|p| p.0).collect::<Vec<_>>());
        let filter_grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        for (row, (title, dropdown)) in [
            ("Priority", &journal_priority),
            ("Boot", &journal_boot),
            ("Time", &journal_since),
        ]
        .into_iter()
        .enumerate()
        {
            let label = gtk::Label::builder().label(title).xalign(0.0).build();
            filter_grid.attach(&label, 0, row as i32, 1, 1);
            filter_grid.attach(dropdown, 1, row as i32, 1, 1);
        }
        let journal_filter_btn = gtk::MenuButton::builder()
            .label("Filters")
            .tooltip_text("Show only some priorities, boots or times")
            .popover(&gtk::Popover::builder().child(&filter_grid).build())
            .build();
        let journal_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        journal_bar.append(&journal_lines);
        journal_bar.append(&journal_filter_btn);
        journal_bar.append(&journal_pause);
        journal_bar.append(&journal_search);
        journal_bar.append(&journal_matches_label);
        journal_bar.append(&linked(&[&journal_prev, &journal_next]));
        journal_bar.append(&journal_reload);
        journal_bar.append(&journal_save);
        let journal_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        journal_box.append(&journal_bar);
        journal_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        journal_box.append(&scrolled(&journal_view));

        // ---- Unit file tab -------------------------------------------------
        let unit_view = unit_file_view(true);
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
        let unit_revert = gtk::Button::builder()
            .label("Revert to Vendor")
            .css_classes(["destructive-action"])
            .visible(false)
            .tooltip_text("Remove your local overrides and go back to the packaged unit")
            .build();
        let unit_original_btn = gtk::ToggleButton::builder()
            .label("Show Original")
            .visible(false)
            .tooltip_text("Show the packaged unit file and its other drop-ins (read only)")
            .build();
        unit_bar.append(&unit_path_label);
        unit_bar.append(&unit_original_btn);
        unit_bar.append(&unit_revert);
        unit_bar.append(&unit_save);
        let unit_original_view = unit_file_view(false);
        let unit_stack = gtk::Stack::new();
        unit_stack.add_named(&scrolled(&unit_view), Some("edit"));
        unit_stack.add_named(&scrolled(&unit_original_view), Some("original"));
        let unit_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        unit_box.append(&unit_banner);
        unit_box.append(&unit_bar);
        unit_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        unit_box.append(&unit_stack);

        // ---- Content pane --------------------------------------------------
        let view_stack = adw::ViewStack::new();
        view_stack.add_titled_with_icon(
            &status_page,
            Some("status"),
            "Status",
            "dialog-information-symbolic",
        );
        let resources = ResourcesView::new();
        view_stack.add_titled_with_icon(
            &resources.widget,
            Some("resources"),
            "Resources",
            "power-profile-performance-symbolic",
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
        list.set_factory(Some(&service_row_factory(&split)));
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
            system_btn: system_btn.clone(),
            user_btn,
            toasts,
            split,
            list,
            store,
            list_filter,
            selection,
            search,
            filter,
            count_label,
            content_page,
            content_stack,
            view_stack,
            resources,
            sampling: Cell::new(false),
            name_label,
            description_label,
            rows,
            buttons,
            journal_view,
            journal_pause,
            journal_search,
            journal_matches_label,
            journal_lines,
            journal_filter_btn,
            journal_priority,
            journal_boot,
            journal_since,
            unit_view,
            unit_banner,
            unit_path_label,
            unit_save,
            unit_revert,
            unit_original_btn,
            unit_original_view,
            unit_stack,
            bus: Cell::new(Bus::System),
            services: RefCell::new(Vec::new()),
            selected: RefCell::new(None),
            details: RefCell::new(None),
            journal: RefCell::new(None),
            journal_gen: Cell::new(0),
            journal_held: RefCell::new(String::new()),
            journal_matches: Cell::new(0),
            list_gen: Cell::new(0),
            rebuilding: Cell::new(false),
            unit_loading: Cell::new(false),
            unit_dirty: Cell::new(false),
            pending_select: RefCell::new(None),
            refresh_pending: Cell::new(false),
        });

        // ---- Signals -------------------------------------------------------
        let weak = Rc::downgrade(&w);
        system_btn.connect_toggled(move |btn| {
            let Some(w) = weak.upgrade() else { return };
            let bus = if btn.is_active() {
                Bus::System
            } else {
                Bus::User
            };
            if w.bus.get() == bus {
                return;
            }
            w.confirm_discard(
                move |w| w.set_bus(bus),
                // Put the toggle back on the bus we are still showing.
                |w| match w.bus.get() {
                    Bus::System => w.system_btn.set_active(true),
                    Bus::User => w.user_btn.set_active(true),
                },
            );
        });
        let weak = Rc::downgrade(&w);
        timers_btn.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                timers::show(&w);
            }
        });
        let weak = Rc::downgrade(&w);
        new_btn.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                new_service::show(&w);
            }
        });

        let weak = Rc::downgrade(&w);
        w.list_filter.set_filter_func(move |obj| {
            let Some(w) = weak.upgrade() else { return true };
            obj.downcast_ref::<glib::BoxedAnyObject>()
                .is_none_or(|b| w.matches(&b.borrow::<Service>()))
        });
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
        w.selection.connect_selected_item_notify(move |sel| {
            let Some(w) = weak.upgrade() else { return };
            if w.rebuilding.get() {
                return;
            }
            let name = sel
                .selected_item()
                .and_downcast::<glib::BoxedAnyObject>()
                .map(|b| b.borrow::<Service>().name.clone());
            let Some(name) = name else { return };
            if w.selected.borrow().as_deref() == Some(name.as_str()) {
                return;
            }
            w.confirm_discard(move |w| w.select(name), |w| w.reselect_current());
        });
        let weak = Rc::downgrade(&w);
        w.list.connect_activate(move |_, _| {
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
            (&b.reset_failed, Action::ResetFailed),
        ] {
            let weak = Rc::downgrade(&w);
            btn.connect_clicked(move |_| {
                if let Some(w) = weak.upgrade() {
                    w.run(action);
                }
            });
        }

        let weak = Rc::downgrade(&w);
        b.delete.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.delete_selected();
            }
        });

        let weak = Rc::downgrade(&w);
        w.journal_pause.connect_toggled(move |btn| {
            if let Some(w) = weak.upgrade() {
                if !btn.is_active() {
                    let held = w.journal_held.take();
                    if !held.is_empty() {
                        w.append_journal(&held);
                    }
                }
                w.update_pause_label();
            }
        });
        let weak = Rc::downgrade(&w);
        w.journal_search.connect_search_changed(move |_| {
            if let Some(w) = weak.upgrade() {
                w.highlight_all();
                w.find_journal(true, true);
            }
        });
        for (signal_btn, forward) in [(&journal_next, true), (&journal_prev, false)] {
            let weak = Rc::downgrade(&w);
            signal_btn.connect_clicked(move |_| {
                if let Some(w) = weak.upgrade() {
                    w.find_journal(forward, false);
                }
            });
        }
        let weak = Rc::downgrade(&w);
        w.journal_search.connect_activate(move |_| {
            if let Some(w) = weak.upgrade() {
                w.find_journal(true, false);
            }
        });
        let weak = Rc::downgrade(&w);
        w.journal_search.connect_next_match(move |_| {
            if let Some(w) = weak.upgrade() {
                w.find_journal(true, false);
            }
        });
        let weak = Rc::downgrade(&w);
        w.journal_search.connect_previous_match(move |_| {
            if let Some(w) = weak.upgrade() {
                w.find_journal(false, false);
            }
        });
        // Shift+Enter goes to the previous match.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&w);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let shift_enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                && mods.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            if shift_enter {
                if let Some(w) = weak.upgrade() {
                    w.find_journal(false, false);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        w.journal_search.add_controller(keys);
        for dropdown in [
            &w.journal_lines,
            &w.journal_priority,
            &w.journal_boot,
            &w.journal_since,
        ] {
            let weak = Rc::downgrade(&w);
            dropdown.connect_selected_notify(move |_| {
                if let Some(w) = weak.upgrade() {
                    w.restart_journal();
                }
            });
        }
        let weak = Rc::downgrade(&w);
        journal_save.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.save_journal();
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
                    w.unit_dirty.set(true);
                }
            }
        });
        let weak = Rc::downgrade(&w);
        w.unit_save.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.save_unit();
            }
        });
        let weak = Rc::downgrade(&w);
        w.unit_revert.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.revert_unit();
            }
        });
        let weak = Rc::downgrade(&w);
        w.unit_original_btn.connect_toggled(move |btn| {
            if let Some(w) = weak.upgrade() {
                w.unit_stack.set_visible_child_name(if btn.is_active() {
                    "original"
                } else {
                    "edit"
                });
            }
        });

        // The window owns the only strong reference to our state.
        let strong = w.clone();
        w.window.connect_close_request(move |_| {
            if strong.unit_dirty.get() {
                strong.confirm_discard(
                    |w| {
                        w.unit_dirty.set(false);
                        w.window.close();
                    },
                    |_| {},
                );
                return glib::Propagation::Stop;
            }
            strong.journal.replace(None);
            glib::Propagation::Proceed
        });

        w.add_shortcuts(app);
        w.refresh_list();
        w.watch_systemd();
        let weak = Rc::downgrade(&w);
        glib::timeout_add_seconds_local(resources::INTERVAL_SECS, move || match weak.upgrade() {
            Some(w) => {
                w.sample_usage();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        w
    }

    /// Every few seconds: samples the selected service's CPU, memory and tasks for the
    /// Resources tab (with its processes when that tab is shown) and the Status tab.
    fn sample_usage(self: &Rc<Self>) {
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        if self.sampling.get() || !self.window.is_visible() {
            return;
        }
        let tab = self.view_stack.visible_child_name();
        let with_processes = tab.as_deref() == Some("resources");
        self.sampling.set(true);
        let bus = self.bus.get();
        let weak = Rc::downgrade(self);
        background(
            move || systemd::usage(bus, &name, with_processes).map(|u| (name, u)),
            move |res| {
                let Some(w) = weak.upgrade() else { return };
                w.sampling.set(false);
                let Ok((name, u)) = res else { return };
                if w.bus.get() != bus || w.selected.borrow().as_deref() != Some(name.as_str()) {
                    return;
                }
                w.resources.push(&name, &u, std::time::Instant::now());
                let dash = || "—".to_string();
                w.rows.memory.set_subtitle(
                    &u.memory
                        .map(|m| glib::format_size(m).to_string())
                        .unwrap_or_else(dash),
                );
                w.rows
                    .cpu
                    .set_subtitle(&u.cpu_nsec.map(systemd::format_cpu).unwrap_or_else(dash));
                w.rows
                    .tasks
                    .set_subtitle(&u.tasks.map(|t| t.to_string()).unwrap_or_else(dash));
            },
        );
    }

    /// Window actions with keyboard shortcuts, and the shortcuts window (Ctrl+?).
    fn add_shortcuts(self: &Rc<Self>, app: &adw::Application) {
        type Handler = fn(&Rc<Window>);
        let actions: [(&str, &[&str], Handler); 17] = [
            ("new-service", &["<Ctrl>n"], |w| new_service::show(w)),
            ("timers", &["<Ctrl>t"], |w| timers::show(w)),
            ("find", &["<Ctrl>f"], |w| w.focus_search()),
            ("refresh", &["F5"], |w| {
                w.refresh_list();
                w.load_details(false);
            }),
            ("save", &["<Ctrl>s"], |w| {
                if w.unit_save.is_sensitive() {
                    w.save_unit();
                }
            }),
            ("start", &["<Ctrl><Shift>s"], |w| {
                w.press(&w.buttons.start, Action::Start)
            }),
            ("stop", &["<Ctrl><Shift>x"], |w| {
                w.press(&w.buttons.stop, Action::Stop)
            }),
            ("restart", &["<Ctrl>r"], |w| {
                w.press(&w.buttons.restart, Action::Restart)
            }),
            ("pause-journal", &["<Ctrl>p"], |w| {
                w.journal_pause.set_active(!w.journal_pause.is_active())
            }),
            ("tab-status", &["<Alt>1"], |w| w.show_tab("status")),
            ("tab-resources", &["<Alt>2"], |w| w.show_tab("resources")),
            ("tab-journal", &["<Alt>3"], |w| w.show_tab("journal")),
            ("tab-unit", &["<Alt>4"], |w| w.show_tab("unit")),
            ("system-bus", &["<Ctrl>1"], |w| {
                w.system_btn.set_active(true)
            }),
            ("user-bus", &["<Ctrl>2"], |w| w.user_btn.set_active(true)),
            ("close", &["<Ctrl>w"], |w| w.window.close()),
            ("about", &[], |w| w.show_about()),
        ];
        for (name, accels, handler) in actions {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(w) = weak.upgrade() {
                    handler(&w);
                }
            });
            self.window.add_action(&action);
            if !accels.is_empty() {
                app.set_accels_for_action(&format!("win.{name}"), accels);
            }
        }
        app.set_accels_for_action("win.show-help-overlay", &["<Ctrl>question"]);
        let builder = gtk::Builder::from_string(SHORTCUTS_UI);
        if let Some(overlay) = builder.object::<gtk::ShortcutsWindow>("help_overlay") {
            self.window.set_help_overlay(Some(&overlay));
        }
    }

    /// Runs an action from a shortcut, only when its button is available.
    fn press(self: &Rc<Self>, button: &gtk::Button, action: Action) {
        let shown = self.content_stack.visible_child_name().as_deref() == Some("details");
        if shown && button.is_visible() && button.is_sensitive() {
            self.run(action);
        }
    }

    /// Ctrl+F: the log search on the Journal tab, otherwise the service search.
    fn focus_search(&self) {
        let details = self.content_stack.visible_child_name().as_deref() == Some("details");
        if details && self.view_stack.visible_child_name().as_deref() == Some("journal") {
            self.journal_search.grab_focus();
        } else {
            self.split.set_show_content(false);
            self.search.grab_focus();
        }
    }

    fn show_tab(&self, name: &str) {
        if self.content_stack.visible_child_name().as_deref() == Some("details") {
            self.view_stack.set_visible_child_name(name);
            self.split.set_show_content(true);
        }
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Systemd Manager")
            .application_icon(crate::APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments(env!("CARGO_PKG_DESCRIPTION"))
            .developer_name("Truong Nguyen")
            .license_type(gtk::License::MitX11)
            .website("https://github.com/Truongnt2812/systemd-manager")
            .issue_url("https://github.com/Truongnt2812/systemd-manager/issues")
            .build();
        about.present(Some(&self.window));
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
        self.clear_selection();
        self.services.borrow_mut().clear();
        self.store.remove_all();
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
                systemd::watch_forever(bus, || {
                    let _ = tx.try_send(());
                });
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
        merge_services(&self.store, &services);
        self.services.replace(services);
        self.rebuilding.set(false);
        self.update_count();

        let Some(name) = self.pending_select.take() else {
            self.reselect_current();
            return;
        };
        // A service we were asked to show (e.g. one just created) may be hidden by
        // the search or filter; clear them so it can be selected.
        if self.position_of(&name).is_none()
            && self.services.borrow().iter().any(|s| s.name == name)
        {
            self.search.set_text("");
            self.filter.set_selected(0);
            self.refilter();
        }
        self.confirm_discard(
            move |w| {
                w.select(name);
                w.reselect_current();
                if let Some(pos) = w
                    .selected
                    .borrow()
                    .as_deref()
                    .and_then(|n| w.position_of(n))
                {
                    w.list.scroll_to(pos, gtk::ListScrollFlags::FOCUS, None);
                }
                w.split.set_show_content(true);
            },
            |w| w.reselect_current(),
        );
    }

    /// Position of a service among the rows shown (after search and filter).
    fn position_of(&self, name: &str) -> Option<u32> {
        (0..self.selection.n_items()).find(|&i| {
            self.selection
                .item(i)
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|b| b.borrow::<Service>().name == name)
        })
    }

    /// Highlights the selected service's row again, e.g. after a refresh, a new
    /// search, or when the user cancels switching to another service.
    fn reselect_current(&self) {
        let pos = self
            .selected
            .borrow()
            .as_deref()
            .and_then(|n| self.position_of(n))
            .unwrap_or(gtk::INVALID_LIST_POSITION);
        if self.selection.selected() == pos {
            return;
        }
        self.rebuilding.set(true);
        self.selection.set_selected(pos);
        self.rebuilding.set(false);
    }

    /// Runs `proceed` right away when the unit file editor has no unsaved edits.
    /// Otherwise asks first, then runs `proceed` on Discard or `cancel` on Cancel.
    fn confirm_discard(
        self: &Rc<Self>,
        proceed: impl FnOnce(&Rc<Self>) + 'static,
        cancel: impl FnOnce(&Rc<Self>) + 'static,
    ) {
        if !self.unit_dirty.get() {
            proceed(self);
            return;
        }
        let name = self.selected.borrow().clone().unwrap_or_default();
        let dialog = adw::AlertDialog::new(
            Some("Discard Unsaved Changes?"),
            Some(&format!(
                "Your edits to the unit file of {name} have not been saved."
            )),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("discard", "_Discard")]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.choose(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |response| {
                let Some(w) = weak.upgrade() else { return };
                if response == "discard" {
                    w.unit_dirty.set(false);
                    proceed(&w);
                } else {
                    cancel(&w);
                }
            },
        );
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
        self.rebuilding.set(true);
        self.list_filter.changed(gtk::FilterChange::Different);
        self.rebuilding.set(false);
        self.update_count();
        self.reselect_current();
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
        self.resources.clear(&name);
        self.load_details(true);
        self.restart_journal();
        self.sample_usage();
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
        self.rows
            .result
            .set_subtitle(&systemd::describe_result(&d.result));
        for class in ["success", "error"] {
            self.rows.result.remove_css_class(class);
        }
        let failed_result = !d.result.is_empty() && d.result != "success";
        if failed_result {
            self.rows.result.add_css_class("error");
        }
        let exit = systemd::describe_exit(d.exec_main_code, d.exec_main_status);
        self.rows.last_exit.set_visible(exit.is_some());
        self.rows.last_exit.set_subtitle(&exit.unwrap_or_default());
        self.rows.cpu.set_subtitle(
            &d.cpu_nsec
                .map(systemd::format_cpu)
                .unwrap_or_else(|| "—".into()),
        );
        self.rows
            .tasks
            .set_subtitle(&d.tasks.map(|t| t.to_string()).unwrap_or_else(|| "—".into()));
        self.rows.restarts.set_subtitle(&d.n_restarts.to_string());
        self.rows
            .triggered_by
            .set_visible(!d.triggered_by.is_empty());
        self.rows
            .triggered_by
            .set_subtitle(&d.triggered_by.join(", "));

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
        b.reset_failed
            .set_visible(d.active_state == "failed" || failed_result || d.n_restarts > 0);
        b.delete.set_visible(systemd::is_deletable(
            self.bus.get(),
            &name,
            &d.fragment_path,
        ));
    }

    /// Asks, then stops, disables and deletes the selected (locally created) service.
    fn delete_selected(self: &Rc<Self>) {
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let Some((fragment, running)) = self.details.borrow().as_ref().map(|d| {
            (
                d.fragment_path.clone(),
                matches!(
                    d.active_state.as_str(),
                    "active" | "reloading" | "activating"
                ),
            )
        }) else {
            return;
        };
        let bus = self.bus.get();
        let files: Vec<String> = systemd::unit_files(bus, &name, &fragment)
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete {name}?")),
            Some(&format!(
                "The service is stopped and disabled, and these files are deleted. This cannot be undone.\n\n{}",
                files.join("\n")
            )),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.choose(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |response| {
                if response != "delete" {
                    return;
                }
                let deleted = name.clone();
                background(
                    move || systemd::delete_service(bus, &name, &fragment, running),
                    move |res| {
                        let Some(w) = weak.upgrade() else { return };
                        match res {
                            Ok(()) => {
                                w.toast(&format!("Deleted {deleted}"));
                                if w.selected.borrow().as_deref() == Some(deleted.as_str()) {
                                    w.clear_selection();
                                }
                                w.refresh_list();
                            }
                            Err(e) => {
                                w.toast(&format!("Could not delete {deleted}: {e}"));
                                w.load_details(false);
                            }
                        }
                    },
                );
            },
        );
    }

    /// Shows the "No Service Selected" page.
    fn clear_selection(&self) {
        self.selected.replace(None);
        self.details.replace(None);
        self.journal.replace(None);
        self.resources.clear("");
        self.unit_dirty.set(false);
        self.content_stack.set_visible_child_name("empty");
        self.content_page.set_title("Service");
        self.split.set_show_content(false);
        self.reselect_current();
    }

    /// Runs `action` on the selected service, asking first when it is risky.
    fn run(self: &Rc<Self>, action: Action) {
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let running = self.details.borrow().as_ref().is_some_and(|d| {
            matches!(
                d.active_state.as_str(),
                "active" | "reloading" | "activating"
            )
        });
        let Some(ask) = systemd::confirmation(self.bus.get(), &name, action, running) else {
            self.run_now(name, action);
            return;
        };
        let dialog = adw::AlertDialog::new(Some(&ask.heading), Some(&ask.body));
        dialog.add_responses(&[("cancel", "_Cancel"), ("go", ask.verb)]);
        dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.choose(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |response| {
                if let (Some(w), "go") = (weak.upgrade(), response.as_str()) {
                    w.run_now(name, action);
                }
            },
        );
    }

    fn run_now(self: &Rc<Self>, name: String, action: Action) {
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

    /// Streams the selected unit's log live (journalctl -f).
    fn restart_journal(self: &Rc<Self>) {
        self.journal.replace(None);
        let generation = self.journal_gen.get() + 1;
        self.journal_gen.set(generation);
        self.journal_held.borrow_mut().clear();
        self.journal_pause.set_active(false);
        self.journal_matches.set(0);
        self.update_matches_label();
        let buffer = self.journal_view.buffer();
        buffer.set_text("");
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let lines = JOURNAL_LINES
            .get(self.journal_lines.selected() as usize)
            .copied()
            .unwrap_or(500);
        let filter = self.journal_filter();
        self.journal_filter_btn
            .set_label(&match filter.active_count() {
                0 => "Filters".to_string(),
                n => format!("Filters ({n})"),
            });
        match journal::spawn(self.bus.get(), &name, lines, true, filter) {
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
                        if w.journal_pause.is_active() {
                            w.journal_held.borrow_mut().push_str(&chunk);
                            w.update_pause_label();
                        } else {
                            w.append_journal(&chunk);
                        }
                    }
                });
            }
        }
    }

    fn journal_filter(&self) -> journal::Filter {
        journal::Filter {
            priority: JOURNAL_PRIORITIES
                .get(self.journal_priority.selected() as usize)
                .and_then(|p| p.1),
            this_boot: self.journal_boot.selected() == 1,
            since: JOURNAL_SINCE
                .get(self.journal_since.selected() as usize)
                .and_then(|p| p.1),
        }
    }

    /// Asks for a file name and writes the log shown (plus lines held while paused).
    fn save_journal(self: &Rc<Self>) {
        let Some(name) = self.selected.borrow().clone() else {
            return;
        };
        let buffer = self.journal_view.buffer();
        let mut text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        text.push_str(&self.journal_held.borrow());
        let dialog = gtk::FileDialog::builder()
            .title("Save Log")
            .initial_name(format!("{}.log", name.trim_end_matches(".service")))
            .build();
        let weak = Rc::downgrade(self);
        dialog.save(Some(&self.window), None::<&gio::Cancellable>, move |res| {
            let Some(w) = weak.upgrade() else { return };
            // An error here usually means the dialog was dismissed.
            let Some(path) = res.ok().and_then(|f| f.path()) else {
                return;
            };
            match std::fs::write(&path, text) {
                Ok(()) => w.toast(&format!("Saved log to {}", path.display())),
                Err(e) => w.toast(&format!("Could not save {}: {e}", path.display())),
            }
        });
    }

    fn update_pause_label(&self) {
        let held = self.journal_held.borrow().matches('\n').count();
        let (label, icon) = match (self.journal_pause.is_active(), held) {
            (false, _) => ("Pause".to_string(), "media-playback-pause-symbolic"),
            (true, 0) => ("Resume".to_string(), "media-playback-start-symbolic"),
            (true, n) => (format!("Resume ({n} new)"), "media-playback-start-symbolic"),
        };
        if let Some(content) = self
            .journal_pause
            .child()
            .and_downcast::<adw::ButtonContent>()
        {
            content.set_label(&label);
            content.set_icon_name(icon);
        }
    }

    fn append_journal(&self, text: &str) {
        let buffer = self.journal_view.buffer();
        let start_offset = buffer.end_iter().offset();
        buffer.insert(&mut buffer.end_iter(), text);
        let excess = buffer.line_count() - JOURNAL_MAX_LINES;
        if excess > 0 {
            if let Some(mut cut) = buffer.iter_at_line(excess) {
                buffer.delete(&mut buffer.start_iter(), &mut cut);
            }
        }
        if !self.journal_search.text().is_empty() {
            // Highlight matches in the new lines only; don't scroll away from
            // the match the user is looking at.
            let start = buffer.iter_at_offset(start_offset.min(buffer.end_iter().offset()));
            let found = self.highlight_range(&start, &buffer.end_iter());
            self.journal_matches.set(self.journal_matches.get() + found);
            self.update_matches_label();
            return;
        }
        let mark = buffer
            .mark("end")
            .unwrap_or_else(|| buffer.create_mark(Some("end"), &buffer.end_iter(), false));
        buffer.move_mark(&mark, &buffer.end_iter());
        self.journal_view.scroll_mark_onscreen(&mark);
    }

    // ---- Journal search ----------------------------------------------------

    fn search_flags() -> gtk::TextSearchFlags {
        gtk::TextSearchFlags::CASE_INSENSITIVE | gtk::TextSearchFlags::TEXT_ONLY
    }

    /// Tags every match between `start` and `end`; returns how many there were.
    fn highlight_range(&self, start: &gtk::TextIter, end: &gtk::TextIter) -> usize {
        let query = self.journal_search.text();
        if query.is_empty() {
            return 0;
        }
        let buffer = self.journal_view.buffer();
        let mut from = *start;
        let mut found = 0;
        while let Some((m_start, m_end)) =
            from.forward_search(&query, Self::search_flags(), Some(end))
        {
            buffer.apply_tag_by_name("match", &m_start, &m_end);
            found += 1;
            from = m_end;
        }
        found
    }

    fn highlight_all(&self) {
        let buffer = self.journal_view.buffer();
        let (start, end) = buffer.bounds();
        buffer.remove_tag_by_name("match", &start, &end);
        buffer.remove_tag_by_name("current-match", &start, &end);
        self.journal_matches.set(self.highlight_range(&start, &end));
        self.update_matches_label();
    }

    fn update_matches_label(&self) {
        let label = match (
            self.journal_search.text().is_empty(),
            self.journal_matches.get(),
        ) {
            (true, _) => String::new(),
            (false, 0) => "No matches".into(),
            (false, 1) => "1 match".into(),
            (false, n) => format!("{n} matches"),
        };
        self.journal_matches_label.set_label(&label);
    }

    /// Selects the next (or previous) match after the current selection, wrapping around.
    /// With `from_top`, starts from the beginning of the log.
    fn find_journal(&self, forward: bool, from_top: bool) {
        let query = self.journal_search.text();
        if query.is_empty() {
            return;
        }
        let buffer = self.journal_view.buffer();
        let (sel_start, sel_end) = buffer
            .selection_bounds()
            .unwrap_or_else(|| (buffer.start_iter(), buffer.start_iter()));
        let flags = Self::search_flags();
        let found = if forward {
            let from = if from_top {
                buffer.start_iter()
            } else {
                sel_end
            };
            from.forward_search(&query, flags, None)
                .or_else(|| buffer.start_iter().forward_search(&query, flags, None))
        } else {
            sel_start
                .backward_search(&query, flags, None)
                .or_else(|| buffer.end_iter().backward_search(&query, flags, None))
        };
        if let Some((start, end)) = found {
            let (all_start, all_end) = buffer.bounds();
            buffer.remove_tag_by_name("current-match", &all_start, &all_end);
            buffer.apply_tag_by_name("current-match", &start, &end);
            buffer.select_range(&start, &end);
            self.journal_view
                .scroll_to_mark(&buffer.get_insert(), 0.1, false, 0.0, 0.0);
        }
    }

    // ---- Unit file ---------------------------------------------------------

    fn load_unit_file(&self, name: &str, d: &Details) {
        let buffer = self.unit_view.buffer();
        let fragment = d.fragment_path.as_str();
        let bus = self.bus.get();
        let target = systemd::edit_target(bus, name, fragment);
        self.unit_loading.set(true);
        // Loading a file is not something Ctrl+Z should undo.
        buffer.begin_irreversible_action();
        let mut editable = true;
        let mut drop_in = false;
        if fragment == "/dev/null" || d.load_state == "masked" {
            buffer.set_text("# This service is masked. Unmask it to view or edit its unit file.\n");
            editable = false;
        } else if fragment.is_empty() {
            buffer.set_text("# This unit has no unit file on disk.\n");
            editable = false;
        } else {
            match &target {
                EditTarget::File(_) => match systemd::read_file(fragment) {
                    Ok(text) => buffer.set_text(&text),
                    Err(e) => {
                        buffer.set_text(&format!("# {e}\n"));
                        editable = false;
                    }
                },
                EditTarget::DropIn(path) => {
                    drop_in = true;
                    let text = std::fs::read_to_string(path)
                        .unwrap_or_else(|_| drop_in_template(name, fragment));
                    buffer.set_text(&text);
                    self.unit_original_view
                        .buffer()
                        .set_text(&original_unit_text(fragment, &d.drop_in_paths, path));
                }
            }
        }
        buffer.end_irreversible_action();
        self.unit_loading.set(false);
        self.unit_dirty.set(false);
        self.unit_view.set_editable(editable);
        self.unit_save.set_sensitive(false);
        self.unit_original_btn.set_active(false);
        self.unit_original_btn.set_visible(drop_in);
        self.unit_stack.set_visible_child_name("edit");
        let shown = if drop_in {
            target.path().to_string_lossy().into_owned()
        } else if fragment.is_empty() {
            "—".to_string()
        } else {
            fragment.to_string()
        };
        self.unit_path_label.set_label(&shown);

        let overrides = if editable {
            systemd::local_overrides(bus, name, fragment)
        } else {
            Vec::new()
        };
        self.unit_revert.set_visible(!overrides.is_empty());
        if drop_in {
            self.unit_banner.set_title(&format!(
                "This is a vendor unit. Your changes are saved as a drop-in override at {}, so package updates still apply",
                target.path().display()
            ));
            self.unit_banner.set_revealed(true);
        } else if !overrides.is_empty() {
            self.unit_banner.set_title(
                "This is a local copy that replaces a vendor unit. Package updates to it are ignored until you revert",
            );
            self.unit_banner.set_revealed(true);
        } else {
            self.unit_banner.set_revealed(false);
        }
    }

    /// Asks, then removes the local overrides of the selected vendor unit.
    fn revert_unit(self: &Rc<Self>) {
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
        let files = systemd::local_overrides(bus, &name, &fragment);
        if files.is_empty() {
            return;
        }
        let list: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Revert {name}?")),
            Some(&format!(
                "This deletes your local changes and goes back to the packaged unit:\n\n{}",
                list.join("\n")
            )),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("revert", "_Revert")]);
        dialog.set_response_appearance("revert", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.choose(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |response| {
                if response != "revert" {
                    return;
                }
                background(
                    move || -> Res<()> {
                        systemd::remove_files(bus, &files)?;
                        systemd::daemon_reload(bus)
                    },
                    move |res| {
                        let Some(w) = weak.upgrade() else { return };
                        match res {
                            Ok(()) => {
                                w.toast(&format!("Reverted {name} to the vendor unit"));
                                w.unit_dirty.set(false);
                                w.load_details(true);
                            }
                            Err(e) => w.toast(&format!("Could not revert: {e}")),
                        }
                    },
                );
            },
        );
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
        let target = systemd::edit_target(bus, &name, &fragment)
            .path()
            .to_path_buf();
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

/// Starting text for a new drop-in override of a vendor unit.
fn drop_in_template(name: &str, fragment: &str) -> String {
    format!(
        "# Drop-in override for {name}.\n\
         # Settings here are applied on top of {fragment};\n\
         # anything you don't list keeps its packaged value.\n\
         #\n\
         # Add the section header for each setting, for example:\n\
         #\n\
         # [Service]\n\
         # Environment=DEBUG=1\n\
         #\n\
         # To replace a list setting such as ExecStart, clear it first:\n\
         # ExecStart=\n\
         # ExecStart=/usr/bin/example --new-flag\n"
    )
}

/// The vendor unit file followed by its other drop-ins, like `systemctl cat`.
fn original_unit_text(fragment: &str, drop_ins: &[String], ours: &Path) -> String {
    let mut out = String::new();
    for path in std::iter::once(fragment).chain(
        drop_ins
            .iter()
            .map(String::as_str)
            .filter(|p| Path::new(p) != ours),
    ) {
        if !out.is_empty() {
            out.push('\n');
        }
        out += &format!("# {path}\n");
        match std::fs::read_to_string(path) {
            Ok(text) => out += &text,
            Err(e) => out += &format!("# could not read: {e}\n"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, state: &str) -> Service {
        Service {
            name: name.into(),
            active_state: state.into(),
            ..Default::default()
        }
    }

    fn names(store: &gio::ListStore) -> Vec<String> {
        (0..store.n_items())
            .filter_map(|i| store.item(i).and_downcast::<glib::BoxedAnyObject>())
            .map(|b| {
                let s = b.borrow::<Service>();
                format!("{}:{}", s.name, s.active_state)
            })
            .collect()
    }

    #[test]
    fn merges_services_in_place() {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        merge_services(&store, &[svc("a", "active"), svc("c", "inactive")]);
        assert_eq!(names(&store), ["a:active", "c:inactive"]);
        let a = store.item(0).unwrap();

        // Insert b, change c, remove nothing; a keeps its object.
        merge_services(
            &store,
            &[svc("a", "active"), svc("b", "active"), svc("c", "failed")],
        );
        assert_eq!(names(&store), ["a:active", "b:active", "c:failed"]);
        assert_eq!(store.item(0).unwrap(), a);

        // Remove a and c, add d at the end.
        merge_services(&store, &[svc("b", "active"), svc("d", "active")]);
        assert_eq!(names(&store), ["b:active", "d:active"]);

        merge_services(&store, &[]);
        assert_eq!(store.n_items(), 0);
    }
}
