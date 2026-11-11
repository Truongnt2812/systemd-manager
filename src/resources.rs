//! The Resources tab: live CPU and memory graphs for the selected service, its other
//! counters, and its processes with the heaviest first.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::glib;

use crate::systemd::{self, Usage};

/// How often the tab samples, in seconds.
pub const INTERVAL_SECS: u32 = 2;
/// Samples kept for the graphs: 3 minutes.
const HISTORY: usize = 90;
/// Processes listed at most.
const MAX_PROCESSES: usize = 50;
/// /proc reports CPU time in USER_HZ ticks, which Linux fixes at 100 per second.
const TICKS_PER_SEC: f64 = 100.0;

/// Number of CPU threads, to turn "% of one core" into "% of all CPUs" (what most
/// system monitors show).
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// "43.8 % of one core (0.9 % of all 48 CPUs)".
pub fn describe_cpu(percent_of_core: f64, cpus: usize) -> String {
    if cpus <= 1 {
        return format!("{percent_of_core:.1} %");
    }
    format!(
        "{percent_of_core:.1} % of one core ({:.1} % of all {cpus} CPUs)",
        percent_of_core / cpus as f64
    )
}

/// "561.5 MB (230.7 MB process memory, 330.8 MB file cache)".
pub fn describe_memory(total: u64, anon: Option<u64>, file: Option<u64>) -> String {
    let size = |b: u64| glib::format_size(b).to_string();
    match (anon, file) {
        (Some(a), Some(f)) => format!(
            "{} ({} process memory, {} file cache)",
            size(total),
            size(a),
            size(f)
        ),
        _ => size(total),
    }
}

/// CPU use in percent of one core, from two readings of a CPU-time counter.
pub fn cpu_percent(delta_cpu_nsec: u64, delta_wall_nsec: u64) -> f64 {
    if delta_wall_nsec == 0 {
        return 0.0;
    }
    delta_cpu_nsec as f64 / delta_wall_nsec as f64 * 100.0
}

/// A process row: its usage since the last sample.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessUsage {
    pub pid: u32,
    pub command: String,
    pub cpu: f64,
    pub rss: u64,
}

/// Heaviest first: by CPU, then by memory.
pub fn sort_processes(procs: &mut [ProcessUsage]) {
    procs.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.rss.cmp(&a.rss))
    });
}

#[derive(Default)]
struct History {
    /// The service the samples belong to.
    name: String,
    cpu: VecDeque<f64>,
    memory: VecDeque<f64>,
    last_cpu: Option<(u64, Instant)>,
    last_ticks: HashMap<u32, (u64, Instant)>,
}

pub struct ResourcesView {
    pub widget: gtk::ScrolledWindow,
    cpu_graph: gtk::DrawingArea,
    memory_graph: gtk::DrawingArea,
    cpu_label: gtk::Label,
    memory_label: gtk::Label,
    tasks: adw::ActionRow,
    io_read: adw::ActionRow,
    io_write: adw::ActionRow,
    processes: gtk::ListBox,
    processes_group: adw::PreferencesGroup,
    history: RefCell<History>,
}

fn value_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle("—")
        .use_markup(false)
        .build();
    row.add_css_class("property");
    row
}

fn graph() -> gtk::DrawingArea {
    gtk::DrawingArea::builder()
        .content_height(120)
        .hexpand(true)
        .build()
}

fn graph_card(area: &gtk::DrawingArea, label: &gtk::Label) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["card"])
        .build();
    area.set_margin_top(12);
    area.set_margin_start(12);
    area.set_margin_end(12);
    label.set_margin_start(12);
    label.set_margin_end(12);
    label.set_margin_bottom(12);
    card.append(area);
    card.append(label);
    card
}

fn size(bytes: f64) -> String {
    glib::format_size(bytes.max(0.0) as u64).to_string()
}

/// Draws `values` (oldest first, filling the width from the right) as a filled line.
fn draw_series(
    cr: &gtk::cairo::Context,
    (width, height): (f64, f64),
    values: &VecDeque<f64>,
    max: f64,
    color: (f64, f64, f64),
    caption: &str,
    fg: &gtk::gdk::RGBA,
) {
    let (fr, fg_, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
    // Grid: quarter lines.
    cr.set_source_rgba(fr, fg_, fb, 0.12);
    cr.set_line_width(1.0);
    for i in 1..4 {
        let y = (height * i as f64 / 4.0).round() + 0.5;
        cr.move_to(0.0, y);
        cr.line_to(width, y);
    }
    let _ = cr.stroke();

    // The scale, top left.
    cr.set_source_rgba(fr, fg_, fb, 0.6);
    cr.set_font_size(11.0);
    cr.move_to(4.0, 13.0);
    let _ = cr.show_text(caption);

    if values.len() < 2 || max <= 0.0 {
        return;
    }
    let step = width / (HISTORY - 1) as f64;
    let x0 = width - step * (values.len() - 1) as f64;
    let y = |v: f64| height - (v / max).clamp(0.0, 1.0) * (height - 2.0) - 1.0;
    cr.move_to(x0, height);
    for (i, v) in values.iter().enumerate() {
        cr.line_to(x0 + step * i as f64, y(*v));
    }
    cr.line_to(width, height);
    cr.close_path();
    cr.set_source_rgba(color.0, color.1, color.2, 0.25);
    let _ = cr.fill();
    for (i, v) in values.iter().enumerate() {
        let (x, y) = (x0 + step * i as f64, y(*v));
        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    cr.set_source_rgb(color.0, color.1, color.2);
    cr.set_line_width(2.0);
    let _ = cr.stroke();
}

/// A round upper bound for a graph, so the scale doesn't jump on every sample.
fn nice_max(peak: f64, floor: f64) -> f64 {
    let target = (peak * 1.25).max(floor);
    let magnitude = 10f64.powf(target.log10().floor());
    for m in [1.0, 1.5, 2.0, 2.5, 5.0, 10.0] {
        if magnitude * m >= target {
            return magnitude * m;
        }
    }
    target
}

impl ResourcesView {
    pub fn new() -> Rc<Self> {
        let cpu_graph = graph();
        let memory_graph = graph();
        let label = || {
            gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label"])
                .build()
        };
        let cpu_label = label();
        let memory_label = label();

        let cpu_group = adw::PreferencesGroup::builder()
            .title("CPU")
            .description(format!(
                "Percent of one core, over the last 3 minutes. 100 % is one core fully busy; this computer has {} CPUs",
                cpu_count()
            ))
            .build();
        cpu_group.add(&graph_card(&cpu_graph, &cpu_label));
        let memory_group = adw::PreferencesGroup::builder()
            .title("Memory")
            .description("Everything systemd counts for the service: its processes' memory plus the kernel's cache of files it used, which is freed when needed")
            .build();
        memory_group.add(&graph_card(&memory_graph, &memory_label));

        let tasks = value_row("Tasks");
        let io_read = value_row("Disk read");
        let io_write = value_row("Disk written");
        let counters = adw::PreferencesGroup::new();
        for r in [&tasks, &io_read, &io_write] {
            counters.add(r);
        }

        let processes = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let processes_group = adw::PreferencesGroup::builder()
            .title("Processes")
            .description("Heaviest first")
            .build();
        processes_group.add(&processes);

        let page = adw::PreferencesPage::new();
        let main = adw::PreferencesGroup::new();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 18);
        for g in [&cpu_group, &memory_group, &counters, &processes_group] {
            column.append(g);
        }
        main.add(&column);
        page.add(&main);
        let widget = gtk::ScrolledWindow::builder()
            .child(&page)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let view = Rc::new(ResourcesView {
            widget,
            cpu_graph,
            memory_graph,
            cpu_label,
            memory_label,
            tasks,
            io_read,
            io_write,
            processes,
            processes_group,
            history: RefCell::new(History::default()),
        });

        let weak = Rc::downgrade(&view);
        view.cpu_graph.set_draw_func(move |area, cr, w, h| {
            let Some(v) = weak.upgrade() else { return };
            let h_ = v.history.borrow();
            let peak = h_.cpu.iter().copied().fold(0.0, f64::max);
            let max = nice_max(peak, 5.0);
            draw_series(
                cr,
                (w as f64, h as f64),
                &h_.cpu,
                max,
                (0.21, 0.52, 0.89),
                &format!("{max:.0} %"),
                &area.color(),
            );
        });
        let weak = Rc::downgrade(&view);
        view.memory_graph.set_draw_func(move |area, cr, w, h| {
            let Some(v) = weak.upgrade() else { return };
            let h_ = v.history.borrow();
            let peak = h_.memory.iter().copied().fold(0.0, f64::max);
            let max = nice_max(peak, 1024.0 * 1024.0);
            draw_series(
                cr,
                (w as f64, h as f64),
                &h_.memory,
                max,
                (0.57, 0.25, 0.67),
                &size(max),
                &area.color(),
            );
        });
        view.clear("");
        view
    }

    /// Forgets the samples, e.g. when another service is selected.
    pub fn clear(&self, name: &str) {
        *self.history.borrow_mut() = History {
            name: name.to_string(),
            ..Default::default()
        };
        self.cpu_label.set_label("Collecting…");
        self.memory_label.set_label("Collecting…");
        for r in [&self.tasks, &self.io_read, &self.io_write] {
            r.set_subtitle("—");
        }
        self.processes.remove_all();
        self.processes_group.set_visible(false);
        self.cpu_graph.queue_draw();
        self.memory_graph.queue_draw();
    }

    /// Adds a sample of `name`. Samples of another service are ignored.
    pub fn push(&self, name: &str, u: &Usage, now: Instant) {
        let mut h = self.history.borrow_mut();
        if h.name != name {
            return;
        }
        let cpu = match (u.cpu_nsec, h.last_cpu) {
            (Some(c), Some((prev, at))) if c >= prev => Some(cpu_percent(
                c - prev,
                now.duration_since(at).as_nanos() as u64,
            )),
            _ => None,
        };
        h.last_cpu = u.cpu_nsec.map(|c| (c, now));
        if let Some(c) = cpu {
            h.cpu.push_back(c);
        }
        if let Some(m) = u.memory {
            h.memory.push_back(m as f64);
        }
        while h.cpu.len() > HISTORY {
            h.cpu.pop_front();
        }
        while h.memory.len() > HISTORY {
            h.memory.pop_front();
        }

        // Per-process CPU since the previous sample.
        let mut rows: Vec<ProcessUsage> = u
            .processes
            .iter()
            .map(|p| {
                let cpu = h
                    .last_ticks
                    .get(&p.pid)
                    .filter(|(t, _)| p.cpu_ticks >= *t)
                    .map(|(t, at)| {
                        let secs = now.duration_since(*at).as_secs_f64();
                        if secs > 0.0 {
                            (p.cpu_ticks - t) as f64 / TICKS_PER_SEC / secs * 100.0
                        } else {
                            0.0
                        }
                    })
                    .unwrap_or(0.0);
                ProcessUsage {
                    pid: p.pid,
                    command: p.command.clone(),
                    cpu,
                    rss: p.rss,
                }
            })
            .collect();
        h.last_ticks = u
            .processes
            .iter()
            .map(|p| (p.pid, (p.cpu_ticks, now)))
            .collect();
        sort_processes(&mut rows);
        let cpu_avg = if h.cpu.is_empty() {
            None
        } else {
            Some(h.cpu.iter().sum::<f64>() / h.cpu.len() as f64)
        };
        drop(h);

        self.cpu_label.set_label(&match (cpu, u.cpu_nsec) {
            (Some(c), Some(total)) => format!(
                "{} now · {:.1} % average · {} in total",
                describe_cpu(c, cpu_count()),
                cpu_avg.unwrap_or(c),
                systemd::format_cpu(total)
            ),
            (None, Some(_)) => "Collecting…".into(),
            _ => "Not running, or CPU accounting is off".into(),
        });
        self.memory_label.set_label(&match u.memory {
            Some(m) => {
                let mut s = format!("{} now", describe_memory(m, u.memory_anon, u.memory_file));
                if let Some(p) = u.memory_peak {
                    s += &format!(" · peak {}", size(p as f64));
                }
                s += &match u.memory_max {
                    Some(l) => format!(" · limit {}", size(l as f64)),
                    None => " · no limit".into(),
                };
                s
            }
            None => "Not running, or memory accounting is off".into(),
        });
        let opt =
            |v: Option<u64>, f: &dyn Fn(u64) -> String| v.map(f).unwrap_or_else(|| "—".into());
        self.tasks.set_subtitle(&opt(u.tasks, &|t| t.to_string()));
        self.io_read
            .set_subtitle(&opt(u.io_read, &|b| size(b as f64)));
        self.io_write
            .set_subtitle(&opt(u.io_write, &|b| size(b as f64)));

        self.processes.remove_all();
        self.processes_group.set_visible(!rows.is_empty());
        for p in rows.iter().take(MAX_PROCESSES) {
            let row = adw::ActionRow::builder()
                .title(&p.command)
                .title_lines(1)
                .subtitle(format!(
                    "PID {} · {:.1} % of a core · {}",
                    p.pid,
                    p.cpu,
                    size(p.rss as f64)
                ))
                .use_markup(false)
                .build();
            row.set_tooltip_text(Some(&p.command));
            self.processes.append(&row);
        }
        self.cpu_graph.queue_draw();
        self.memory_graph.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_cpu_percent() {
        assert_eq!(cpu_percent(1_000_000_000, 2_000_000_000), 50.0);
        assert_eq!(cpu_percent(4_000_000_000, 2_000_000_000), 200.0);
        assert_eq!(cpu_percent(5, 0), 0.0);
    }

    #[test]
    fn describes_cpu_and_memory() {
        assert_eq!(
            describe_cpu(43.8, 48),
            "43.8 % of one core (0.9 % of all 48 CPUs)"
        );
        assert_eq!(describe_cpu(5.0, 1), "5.0 %");
        assert!(describe_memory(100, Some(40), Some(60)).contains("process memory"));
        assert!(!describe_memory(100, None, None).contains("cache"));
    }

    #[test]
    fn heaviest_processes_first() {
        let p = |pid, cpu, rss| ProcessUsage {
            pid,
            command: String::new(),
            cpu,
            rss,
        };
        let mut procs = vec![p(1, 0.5, 10), p(2, 9.0, 5), p(3, 0.5, 99)];
        sort_processes(&mut procs);
        assert_eq!(
            procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
    }

    #[test]
    fn round_graph_maxima() {
        assert_eq!(nice_max(3.0, 5.0), 5.0);
        assert_eq!(nice_max(30.0, 5.0), 50.0);
        assert_eq!(nice_max(90.0, 5.0), 150.0);
    }
}
