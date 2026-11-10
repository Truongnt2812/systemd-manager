//! Dialog listing the timers of the current bus, with their next and last run.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::systemd::{self, Timer};
use crate::window::{background, Window};

/// "in 2 h 5 min", "3 min ago", "now".
pub fn relative(delta_us: i64) -> String {
    let secs = delta_us.unsigned_abs() / 1_000_000;
    let span = if secs < 60 {
        return if secs < 5 {
            "now".into()
        } else if delta_us > 0 {
            format!("in {secs} s")
        } else {
            format!("{secs} s ago")
        };
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else if secs < 86_400 {
        format!("{} h {} min", secs / 3600, secs / 60 % 60)
    } else {
        format!("{} d {} h", secs / 86_400, secs / 3600 % 24)
    };
    if delta_us > 0 {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

/// Formats a time in µs since the epoch as "2026-11-10 14:00 (in 2 h 5 min)".
fn when(us: u64) -> String {
    if us == 0 {
        return "—".into();
    }
    let now = glib::real_time();
    let date = glib::DateTime::from_unix_local((us / 1_000_000) as i64)
        .ok()
        .and_then(|t| t.format("%Y-%m-%d %H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    format!("{date} ({})", relative(us as i64 - now))
}

/// The next run in µs since the epoch, also for monotonic (boot-relative) timers.
fn next_run(t: &Timer) -> u64 {
    if t.next_realtime > 0 {
        return t.next_realtime;
    }
    if t.next_monotonic > 0 {
        let from_now = t.next_monotonic as i64 - glib::monotonic_time();
        return (glib::real_time() + from_now).max(0) as u64;
    }
    0
}

fn timer_row(t: &Timer) -> adw::ActionRow {
    let mut subtitle = String::new();
    if !t.description.is_empty() && t.description != t.name {
        subtitle += &format!("{}\n", t.description);
    }
    subtitle += &format!("Runs {}", t.unit);
    if t.active_state != "active" {
        subtitle += &format!(" · {}", t.active_state);
    }
    subtitle += &format!(
        "\nNext: {}\nLast: {}",
        when(next_run(t)),
        when(t.last_trigger)
    );
    let row = adw::ActionRow::builder()
        .title(&t.name)
        .subtitle(&subtitle)
        .subtitle_lines(4)
        .use_markup(false)
        .activatable(t.unit.ends_with(".service"))
        .build();
    if t.unit.ends_with(".service") {
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.set_tooltip_text(Some(&format!("Show {}", t.unit)));
    }
    row
}

pub fn show(win: &Rc<Window>) {
    let bus = win.bus();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .valign(gtk::Align::Start)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("alarm-symbolic")
        .title("No Timers")
        .description(
            "Timers start services on a schedule. Create one with New service → Run on a schedule.",
        )
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build(),
        Some("list"),
    );
    stack.add_named(&empty, Some("empty"));

    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh")
        .build();
    let header = adw::HeaderBar::new();
    header.pack_start(&refresh);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let dialog = adw::Dialog::builder()
        .title(match bus {
            systemd::Bus::System => "System Timers",
            systemd::Bus::User => "User Timers",
        })
        .content_width(560)
        .content_height(560)
        .child(&toolbar)
        .build();

    let load: Rc<dyn Fn()> = {
        let list = list.clone();
        let stack = stack.clone();
        let win = Rc::downgrade(win);
        let dialog = dialog.clone();
        Rc::new(move || {
            let list = list.clone();
            let stack = stack.clone();
            let win = win.clone();
            let dialog = dialog.clone();
            background(
                move || systemd::list_timers(bus),
                move |res| {
                    list.remove_all();
                    let timers = match res {
                        Ok(t) => t,
                        Err(e) => {
                            if let Some(w) = win.upgrade() {
                                w.toast(&format!("Could not list timers: {e}"));
                            }
                            Vec::new()
                        }
                    };
                    stack.set_visible_child_name(if timers.is_empty() { "empty" } else { "list" });
                    for t in &timers {
                        let row = timer_row(t);
                        let unit = t.unit.clone();
                        let win = win.clone();
                        let dialog = dialog.clone();
                        row.connect_activated(move |_| {
                            dialog.close();
                            if let Some(w) = win.upgrade() {
                                w.select_after_refresh(unit.clone());
                            }
                        });
                        list.append(&row);
                    }
                },
            );
        })
    };
    let reload = load.clone();
    refresh.connect_clicked(move |_| reload());
    load();
    dialog.present(Some(&win.window));
}

#[cfg(test)]
mod tests {
    use super::relative;

    #[test]
    fn relative_times() {
        assert_eq!(relative(1_000_000), "now");
        assert_eq!(relative(30_000_000), "in 30 s");
        assert_eq!(relative(-30_000_000), "30 s ago");
        assert_eq!(relative(125 * 60 * 1_000_000), "in 2 h 5 min");
        assert_eq!(relative(-3 * 60 * 1_000_000), "3 min ago");
        assert_eq!(relative(26 * 3600 * 1_000_000), "in 1 d 2 h");
    }
}
