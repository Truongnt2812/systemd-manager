//! Dialog for creating a new .service unit.

use std::rc::Rc;

use adw::prelude::*;

use crate::systemd::{self, Action, Bus, NewService};
use crate::window::{background, Window};

const TYPES: [&str; 5] = ["simple", "exec", "forking", "oneshot", "notify"];
const RESTARTS: [&str; 7] = [
    "no",
    "on-success",
    "on-failure",
    "on-abnormal",
    "on-abort",
    "on-watchdog",
    "always",
];
const RESTART_HELP: [&str; 7] = [
    "Never restart",
    "Restart only after a clean exit",
    "Restart after an error exit, a crash or a timeout",
    "Restart after a crash or a timeout, not after an error exit",
    "Restart only after a crash (uncaught signal)",
    "Restart only after a watchdog timeout",
    "Always restart, however it stopped",
];

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
    let exec_stop = entry("Stop command (ExecStop, optional)");
    let environment = entry("Environment (optional), e.g. PORT=8080 DEBUG=1");
    let workdir = entry("Working directory (optional)");
    let user = entry("Run as user (optional)");
    user.set_visible(system);
    let kind = combo("Type", &TYPES, 0);
    kind.set_subtitle("simple suits most long-running programs");
    let restart = combo("Restart", &RESTARTS, 2);
    restart.set_subtitle(RESTART_HELP[2]);
    let restart_sec = adw::SpinRow::builder()
        .title("Restart delay (seconds)")
        .subtitle("0 uses systemd's default (100 ms)")
        .adjustment(&gtk::Adjustment::new(0.0, 0.0, 3600.0, 1.0, 10.0, 0.0))
        .build();
    {
        let restart_sec = restart_sec.clone();
        restart.connect_selected_notify(move |row| {
            let i = row.selected() as usize;
            row.set_subtitle(RESTART_HELP.get(i).copied().unwrap_or_default());
            restart_sec.set_visible(i != 0);
        });
    }

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
    for row in [
        &name,
        &description,
        &exec,
        &exec_stop,
        &environment,
        &workdir,
        &user,
    ] {
        service_group.add(row);
    }
    service_group.add(&kind);
    service_group.add(&restart);
    service_group.add(&restart_sec);

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
        let exec_start = match systemd::resolve_command(&exec.text()) {
            Ok(c) => c,
            Err(e) => return show_error(&e),
        };
        let stop_text = exec_stop.text();
        let stop_command = if stop_text.trim().is_empty() {
            String::new()
        } else {
            match systemd::resolve_command(&stop_text) {
                Ok(c) => c,
                Err(e) => return show_error(&format!("Stop command: {e}")),
            }
        };
        let env = match systemd::parse_environment(&environment.text()) {
            Ok(e) => e,
            Err(e) => return show_error(&e),
        };
        let service_type = TYPES[kind.selected() as usize];
        let restart_policy = RESTARTS[restart.selected() as usize];
        if service_type == "oneshot" && matches!(restart_policy, "always" | "on-success") {
            return show_error(&format!(
                "Type oneshot can't use Restart={restart_policy}; pick another restart policy"
            ));
        }
        let workdir_text = workdir.text().trim().to_string();
        if !workdir_text.is_empty() && !workdir_text.starts_with('/') && workdir_text != "~" {
            return show_error("The working directory must be an absolute path");
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
            exec_stop: stop_command,
            environment: env,
            restart_sec: restart_sec.value() as u32,
            service_type: service_type.to_string(),
            restart: restart_policy.to_string(),
            working_dir: workdir_text,
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
                let mut files = vec![(path.clone(), content)];
                if let Some(timer) = &timer {
                    files.push((timer_path.clone(), timer.clone()));
                }
                systemd::write_units_and_reload(bus, &files)?;
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
