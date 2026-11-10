//! Dialog for creating a new .service unit.

use std::rc::Rc;

use adw::prelude::*;

use crate::systemd::{self, Action, Bus, NewService};
use crate::window::{background, Window};

const TYPES: [&str; 5] = ["simple", "exec", "forking", "oneshot", "notify"];
const RESTARTS: [&str; 3] = ["no", "on-failure", "always"];

fn entry(title: &str) -> adw::EntryRow {
    adw::EntryRow::builder()
        .title(title)
        .use_markup(false)
        .build()
}

fn combo(title: &str, items: &[&str], selected: u32) -> adw::ComboRow {
    adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(items))
        .selected(selected)
        .build()
}

pub fn show(win: &Rc<Window>) {
    let bus = win.bus();
    let system = bus == Bus::System;

    let create = gtk::Button::builder()
        .label("Create")
        .css_classes(["suggested-action"])
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&create);

    let name = entry("Name, e.g. my-app");
    let description = entry("Description");
    let exec = entry("Command (ExecStart), e.g. /usr/bin/python3 /opt/app/main.py");
    let workdir = entry("Working directory (optional)");
    let user = entry("Run as user (optional)");
    user.set_visible(system);
    let kind = combo("Type", &TYPES, 0);
    kind.set_subtitle("simple suits most long-running programs");
    let restart = combo("Restart", &RESTARTS, 1);

    let wanted_by = entry("Start with target (WantedBy)");
    wanted_by.set_text(bus.default_target());
    let scheduled = adw::SwitchRow::builder()
        .title("Run on a schedule")
        .subtitle("Creates a timer that starts the service, instead of keeping it running")
        .build();
    let schedule = entry("When (OnCalendar), e.g. daily, Mon..Fri 09:00, *:0/15");
    schedule.set_text("daily");
    schedule.set_visible(false);
    let enable = adw::SwitchRow::builder()
        .title(if system {
            "Enable at boot"
        } else {
            "Enable at login"
        })
        .active(true)
        .build();
    let start = adw::SwitchRow::builder()
        .title("Start now")
        .active(true)
        .build();

    let service_group = adw::PreferencesGroup::builder()
        .title("Service")
        .description(format!(
            "The unit file will be created in {}",
            bus.unit_dir().display()
        ))
        .build();
    for row in [&name, &description, &exec, &workdir, &user] {
        service_group.add(row);
    }
    service_group.add(&kind);
    service_group.add(&restart);

    let startup_group = adw::PreferencesGroup::builder().title("Startup").build();
    startup_group.add(&scheduled);
    startup_group.add(&schedule);
    startup_group.add(&wanted_by);
    startup_group.add(&enable);
    startup_group.add(&start);

    // A scheduled job runs once and exits, so suggest oneshot without restarts, and
    // let the timer (not a target) start it.
    {
        let (schedule, wanted_by, kind, restart, enable, start) = (
            schedule.clone(),
            wanted_by.clone(),
            kind.clone(),
            restart.clone(),
            enable.clone(),
            start.clone(),
        );
        let enable_title = enable.title();
        scheduled.connect_active_notify(move |row| {
            let on = row.is_active();
            schedule.set_visible(on);
            wanted_by.set_visible(!on);
            if on {
                enable.set_title(if system {
                    "Enable the timer at boot"
                } else {
                    "Enable the timer at login"
                });
                start.set_title("Start the timer now");
            } else {
                enable.set_title(&enable_title);
                start.set_title("Start now");
            }
            if on {
                kind.set_selected(TYPES.iter().position(|t| *t == "oneshot").unwrap_or(0) as u32);
                restart.set_selected(0);
            }
        });
    }

    let page = adw::PreferencesPage::new();
    page.add(&service_group);
    page.add(&startup_group);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));

    let dialog = adw::Dialog::builder()
        .title(if system {
            "New System Service"
        } else {
            "New User Service"
        })
        .content_width(560)
        .content_height(640)
        .child(&toasts)
        .build();

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });

    let win_weak = Rc::downgrade(win);
    let d = dialog.clone();
    create.connect_clicked(move |btn| {
        let show_error = |msg: &str| {
            let t = adw::Toast::new(msg);
            t.set_use_markup(false);
            toasts.add_toast(t);
        };
        let unit_name = match systemd::normalize_name(&name.text()) {
            Ok(n) => n,
            Err(e) => return show_error(&e),
        };
        let exec_start = exec.text().trim().to_string();
        if exec_start.is_empty() {
            return show_error("Enter the command the service should run");
        }
        let path = bus.unit_dir().join(&unit_name);
        if path.exists() {
            return show_error(&format!("{} already exists", path.display()));
        }
        let when = scheduled
            .is_active()
            .then(|| schedule.text().trim().to_string());
        let timer_name = format!("{}.timer", unit_name.trim_end_matches(".service"));
        let timer_path = bus.unit_dir().join(&timer_name);
        if let Some(when) = &when {
            if when.is_empty() {
                return show_error("Enter when the service should run");
            }
            if let Err(e) = systemd::check_calendar(when) {
                return show_error(&e);
            }
            if timer_path.exists() {
                return show_error(&format!("{} already exists", timer_path.display()));
            }
        }
        let spec = NewService {
            name: unit_name.clone(),
            description: description.text().trim().to_string(),
            exec_start,
            service_type: TYPES[kind.selected() as usize].to_string(),
            restart: RESTARTS[restart.selected() as usize].to_string(),
            working_dir: workdir.text().trim().to_string(),
            user: user.text().trim().to_string(),
            wanted_by: wanted_by.text().trim().to_string(),
            schedule: when,
        };
        let content = systemd::render_unit(bus, &spec);
        let timer = systemd::render_timer(&spec);
        let do_enable = enable.is_active() && (timer.is_some() || !spec.wanted_by.is_empty());
        let do_start = start.is_active();

        btn.set_sensitive(false);
        let btn = btn.clone();
        let d = d.clone();
        let toasts = toasts.clone();
        let win_weak = win_weak.clone();
        let created = unit_name.clone();
        background(
            move || -> systemd::Res<Vec<String>> {
                systemd::write_unit_file(bus, &path, &content)?;
                if let Some(timer) = &timer {
                    systemd::write_unit_file(bus, &timer_path, timer)?;
                }
                systemd::daemon_reload(bus)?;
                // With a schedule, the timer is what gets enabled and started.
                let target = if timer.is_some() {
                    timer_name.as_str()
                } else {
                    unit_name.as_str()
                };
                let mut warnings = Vec::new();
                if do_enable {
                    if let Err(e) = systemd::run_action(bus, target, Action::Enable) {
                        warnings.push(format!("enable failed: {e}"));
                    }
                }
                if do_start {
                    if let Err(e) = systemd::run_action(bus, target, Action::Start) {
                        warnings.push(format!("start failed: {e}"));
                    }
                }
                Ok(warnings)
            },
            move |res| match res {
                Ok(warnings) => {
                    d.close();
                    if let Some(w) = win_weak.upgrade() {
                        let mut msg = format!("Created {created}");
                        if !warnings.is_empty() {
                            msg += &format!(", but {}", warnings.join("; "));
                        }
                        w.toast(&msg);
                        w.select_after_refresh(created);
                    }
                }
                Err(e) => {
                    btn.set_sensitive(true);
                    let t = adw::Toast::new(&e);
                    t.set_use_markup(false);
                    toasts.add_toast(t);
                }
            },
        );
    });

    dialog.present(Some(&win.window));
}
