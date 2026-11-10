//! Thin blocking wrapper around the systemd D-Bus API (org.freedesktop.systemd1).
//!
//! Every function here blocks, so the UI calls them through `gio::spawn_blocking`.
//! Privileged calls on the system bus set the "allow interactive authorization"
//! flag, which lets systemd ask polkit to show a password prompt.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use zbus::blocking::{proxy::Builder, Connection, Proxy};
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::MatchRule;

const DEST: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";

pub type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bus {
    System,
    User,
}

impl Bus {
    fn cached(self) -> &'static Mutex<Option<Connection>> {
        static SYSTEM: Mutex<Option<Connection>> = Mutex::new(None);
        static USER: Mutex<Option<Connection>> = Mutex::new(None);
        match self {
            Bus::System => &SYSTEM,
            Bus::User => &USER,
        }
    }

    /// The shared connection to this bus, opened on first use.
    fn connection(self) -> Res<Connection> {
        let mut cell = self.cached().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cell.as_ref() {
            return Ok(c.clone());
        }
        let c = match self {
            Bus::System => Connection::system(),
            Bus::User => Connection::session(),
        }
        .map_err(err)?;
        *cell = Some(c.clone());
        Ok(c)
    }

    /// Forgets the shared connection, so the next call opens a new one. Used after
    /// the bus went away (e.g. dbus or systemd was restarted).
    pub fn reset_connection(self) {
        self.cached()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    /// Directory where locally administered unit files live.
    pub fn unit_dir(self) -> PathBuf {
        match self {
            Bus::System => PathBuf::from("/etc/systemd/system"),
            Bus::User => {
                let config = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| {
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                    });
                config.join("systemd/user")
            }
        }
    }

    /// Directories that hold unit files installed by packages or generated at runtime.
    /// Units found there are never edited in place.
    pub fn vendor_dirs(self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = match self {
            Bus::System => [
                "/usr/lib/systemd/system",
                "/lib/systemd/system",
                "/usr/local/lib/systemd/system",
                "/run/systemd/system",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
            Bus::User => [
                "/usr/lib/systemd/user",
                "/usr/local/lib/systemd/user",
                "/etc/systemd/user",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
        };
        if self == Bus::User {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share/systemd/user"));
            }
        }
        dirs
    }

    pub fn default_target(self) -> &'static str {
        match self {
            Bus::System => "multi-user.target",
            Bus::User => "default.target",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Service {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    /// enabled / disabled / static / masked / ... (empty for transient units)
    pub file_state: String,
}

#[derive(Clone, Debug, Default)]
pub struct Details {
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub file_state: String,
    pub fragment_path: String,
    /// Drop-in files (`<unit>.d/*.conf`) applied on top of the unit file.
    pub drop_in_paths: Vec<String>,
    pub main_pid: u32,
    pub memory: Option<u64>,
    /// Microseconds since the epoch at which the unit last entered the active state.
    pub active_since: u64,
    /// CPU time used, in nanoseconds (None without CPU accounting).
    pub cpu_nsec: Option<u64>,
    /// Number of tasks (processes and threads) in the unit's cgroup.
    pub tasks: Option<u64>,
    /// How many times systemd restarted the service automatically.
    pub n_restarts: u32,
    /// How the service last ended: "success", "exit-code", "signal", "timeout", ...
    pub result: String,
    /// How the main process last exited (CLD_EXITED = 1, CLD_KILLED = 2, CLD_DUMPED = 3),
    /// and its exit status or signal number.
    pub exec_main_code: i32,
    pub exec_main_status: i32,
    /// Units that start this one, e.g. a timer.
    pub triggered_by: Vec<String>,
}

/// A plain description of a service's Result property.
pub fn describe_result(result: &str) -> String {
    match result {
        "" => "—",
        "success" => "Success",
        "exit-code" => "Failed: the program exited with an error",
        "signal" => "Failed: the program was killed by a signal",
        "core-dump" => "Failed: the program crashed (core dumped)",
        "timeout" => "Failed: timed out",
        "watchdog" => "Failed: watchdog timeout",
        "start-limit-hit" => "Failed: restarted too often (start limit hit)",
        "oom-kill" => "Failed: killed for running out of memory",
        "resources" => "Failed: could not set up the service (missing file, user or directory?)",
        "protocol" => "Failed: the program didn't follow the startup protocol of its Type",
        "exec-condition" => "Skipped: ExecCondition failed",
        other => return format!("Failed: {other}"),
    }
    .to_string()
}

fn signal_name(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// How the main process last exited, e.g. "exit code 1" or "killed by SIGKILL".
pub fn describe_exit(code: i32, status: i32) -> Option<String> {
    let signal = || {
        signal_name(status)
            .map(str::to_string)
            .unwrap_or_else(|| format!("signal {status}"))
    };
    match code {
        1 => Some(format!("exit code {status}")),
        2 => Some(format!("killed by {}", signal())),
        3 => Some(format!("crashed with {} (core dumped)", signal())),
        _ => None,
    }
}

/// CPU time in a readable form: "0.25 s", "3 min 4 s", "2 h 5 min".
pub fn format_cpu(nsec: u64) -> String {
    let secs = nsec as f64 / 1e9;
    if secs < 60.0 {
        return format!("{secs:.2} s");
    }
    let total = secs as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{m} min {s} s")
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
    Mask,
    Unmask,
    ResetFailed,
}

impl Action {
    pub fn past_tense(self) -> &'static str {
        match self {
            Action::Start => "Started",
            Action::Stop => "Stopped",
            Action::Restart => "Restarted",
            Action::Reload => "Reloaded",
            Action::Enable => "Enabled",
            Action::Disable => "Disabled",
            Action::Mask => "Masked",
            Action::Unmask => "Unmasked",
            Action::ResetFailed => "Reset the failed state of",
        }
    }
}

/// Text for a dialog that asks before a risky action.
#[derive(Debug, PartialEq, Eq)]
pub struct Confirmation {
    pub heading: String,
    pub body: String,
    /// Label of the button that goes ahead.
    pub verb: &'static str,
}

/// What stopping a well-known system service breaks, if it is one.
fn critical_role(name: &str) -> Option<&'static str> {
    let stem = name.trim_end_matches(".service");
    Some(match stem {
        "ssh" | "sshd" => "remote SSH logins, including the session you may be using right now",
        "NetworkManager" | "systemd-networkd" | "wpa_supplicant" | "iwd" | "connman" => {
            "your network connection"
        }
        "systemd-resolved" => "DNS name resolution",
        "gdm" | "gdm3" | "sddm" | "lightdm" | "display-manager" => {
            "the login screen and your desktop session"
        }
        "dbus" | "dbus-broker" | "polkit" | "systemd-logind" => {
            "core parts of the system, including this app"
        }
        "systemd-journald" => "system logging",
        "firewalld" | "ufw" | "nftables" => "the firewall",
        _ => return None,
    })
}

/// Returns the question to ask before running `action`, or None when it is safe to
/// go ahead. Only system services are guarded: masking any of them, and stopping,
/// restarting or disabling the ones a working system depends on.
pub fn confirmation(bus: Bus, name: &str, action: Action, running: bool) -> Option<Confirmation> {
    if bus != Bus::System {
        return None;
    }
    let role = critical_role(name);
    let warning = role
        .map(|r| format!("\n\n{name} provides {r}."))
        .unwrap_or_default();
    let (heading, body, verb) = match action {
        Action::Mask => (
            format!("Mask {name}?"),
            format!(
                "A masked service cannot be started at all, not even as a dependency of another service, until you unmask it.{warning}"
            ),
            "Mask",
        ),
        Action::Stop if running && role.is_some() => (
            format!("Stop {name}?"),
            format!("Stopping it can cut you off from {}.", role?),
            "Stop",
        ),
        Action::Restart if running && role.is_some() => (
            format!("Restart {name}?"),
            format!(
                "While it restarts you can briefly lose {}. If it fails to come back, you lose it until it is started again.",
                role?
            ),
            "Restart",
        ),
        Action::Disable if role.is_some() => (
            format!("Disable {name}?"),
            format!(
                "It will no longer start at boot, so after the next restart you will be without {}.",
                role?
            ),
            "Disable",
        ),
        _ => return None,
    };
    Some(Confirmation {
        heading,
        body,
        verb,
    })
}

fn proxy(conn: &Connection, path: impl Into<String>, iface: &'static str) -> Res<Proxy<'static>> {
    let path = zbus::zvariant::ObjectPath::try_from(path.into()).map_err(err)?;
    Builder::<Proxy>::new(conn)
        .destination(DEST)
        .map_err(err)?
        .path(path)
        .map_err(err)?
        .interface(iface)
        .map_err(err)?
        .cache_properties(CacheProperties::No)
        .build()
        .map_err(err)
}

fn manager(bus: Bus) -> Res<Proxy<'static>> {
    proxy(&bus.connection()?, MANAGER_PATH, MANAGER_IFACE)
}

/// Calls a method allowing polkit to prompt the user for a password.
fn call_auth<B, R>(p: &Proxy, method: &str, body: &B) -> Res<R>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    p.call_with_flags(method, MethodFlags::AllowInteractiveAuth.into(), body)
        .map_err(err)?
        .ok_or_else(|| format!("{method}: no reply from systemd"))
}

type UnitRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// All services: the ones systemd has loaded plus installed unit files that aren't loaded.
pub fn list_services(bus: Bus) -> Res<Vec<Service>> {
    let m = manager(bus)?;
    let units: Vec<UnitRow> = m.call("ListUnits", &()).map_err(err)?;
    let files: Vec<(String, String)> = m.call("ListUnitFiles", &()).map_err(err)?;

    let mut map: BTreeMap<String, Service> = BTreeMap::new();
    for u in units {
        if !u.0.ends_with(".service") {
            continue;
        }
        map.insert(
            u.0.clone(),
            Service {
                name: u.0,
                description: u.1,
                load_state: u.2,
                active_state: u.3,
                sub_state: u.4,
                file_state: String::new(),
            },
        );
    }
    for (path, state) in files {
        let Some(name) = Path::new(&path).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".service") {
            continue;
        }
        let entry = map.entry(name.to_string()).or_insert_with(|| Service {
            name: name.to_string(),
            load_state: "not-loaded".into(),
            active_state: "inactive".into(),
            sub_state: "dead".into(),
            ..Default::default()
        });
        if entry.file_state.is_empty() {
            entry.file_state = state;
        }
    }
    let mut services: Vec<Service> = map.into_values().collect();
    services.sort_by_key(|s| s.name.to_lowercase());
    Ok(services)
}

pub fn details(bus: Bus, name: &str) -> Res<Details> {
    let conn = bus.connection()?;
    let m = manager(bus)?;
    let path: OwnedObjectPath = m.call("LoadUnit", &(name,)).map_err(err)?;
    let unit = proxy(&conn, path.as_str(), "org.freedesktop.systemd1.Unit")?;
    let svc = proxy(&conn, path.as_str(), "org.freedesktop.systemd1.Service")?;

    let s = |p: &str| unit.get_property::<String>(p).unwrap_or_default();
    // u64::MAX means "not known" (accounting off or the unit isn't running).
    let known = |p: &str| svc.get_property::<u64>(p).ok().filter(|v| *v != u64::MAX);
    let memory = known("MemoryCurrent");
    Ok(Details {
        description: s("Description"),
        load_state: s("LoadState"),
        active_state: s("ActiveState"),
        sub_state: s("SubState"),
        file_state: s("UnitFileState"),
        fragment_path: s("FragmentPath"),
        drop_in_paths: unit
            .get_property::<Vec<String>>("DropInPaths")
            .unwrap_or_default(),
        main_pid: svc.get_property::<u32>("MainPID").unwrap_or(0),
        memory,
        active_since: unit
            .get_property::<u64>("ActiveEnterTimestamp")
            .unwrap_or(0),
        cpu_nsec: known("CPUUsageNSec"),
        tasks: known("TasksCurrent"),
        n_restarts: svc.get_property::<u32>("NRestarts").unwrap_or(0),
        result: svc.get_property::<String>("Result").unwrap_or_default(),
        exec_main_code: svc.get_property::<i32>("ExecMainCode").unwrap_or(0),
        exec_main_status: svc.get_property::<i32>("ExecMainStatus").unwrap_or(0),
        triggered_by: unit
            .get_property::<Vec<String>>("TriggeredBy")
            .unwrap_or_default(),
    })
}

#[derive(Clone, Debug, Default)]
pub struct Timer {
    pub name: String,
    pub description: String,
    pub active_state: String,
    /// The unit the timer starts.
    pub unit: String,
    /// Next elapse, in µs since the epoch (calendar timers), 0 if none.
    pub next_realtime: u64,
    /// Next elapse, in µs of CLOCK_MONOTONIC (OnBootSec= and similar timers), 0 if none.
    pub next_monotonic: u64,
    /// Last time the timer fired, in µs since the epoch, 0 if never.
    pub last_trigger: u64,
}

/// All loaded timers, sorted by name.
pub fn list_timers(bus: Bus) -> Res<Vec<Timer>> {
    let conn = bus.connection()?;
    let m = manager(bus)?;
    let no_states: Vec<String> = Vec::new();
    let units: Vec<UnitRow> = m
        .call("ListUnitsByPatterns", &(no_states, vec!["*.timer"]))
        .map_err(err)?;
    let mut timers = Vec::new();
    for u in units {
        let t = proxy(&conn, u.6.as_str(), "org.freedesktop.systemd1.Timer")?;
        // u64::MAX means "never" for these properties.
        let time = |p: &str| {
            t.get_property::<u64>(p)
                .ok()
                .filter(|v| *v != u64::MAX)
                .unwrap_or(0)
        };
        timers.push(Timer {
            unit: t.get_property::<String>("Unit").unwrap_or_default(),
            next_realtime: time("NextElapseUSecRealtime"),
            next_monotonic: time("NextElapseUSecMonotonic"),
            last_trigger: time("LastTriggerUSec"),
            name: u.0,
            description: u.1,
            active_state: u.3,
        });
    }
    timers.sort_by_key(|t| t.name.to_lowercase());
    Ok(timers)
}

/// Checks an OnCalendar= expression with `systemd-analyze calendar`, when available.
pub fn check_calendar(expr: &str) -> Res<()> {
    match Command::new("systemd-analyze")
        .args(["calendar", "--", expr])
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) if !out.status.success() => Err(format!(
            "\"{expr}\" is not a valid schedule: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        // Fine, or systemd-analyze isn't installed: let systemd judge it later.
        _ => Ok(()),
    }
}

pub fn daemon_reload(bus: Bus) -> Res<()> {
    call_auth(&manager(bus)?, "Reload", &())
}

/// Runs an action and returns a short message describing the outcome.
pub fn run_action(bus: Bus, name: &str, action: Action) -> Res<String> {
    let m = manager(bus)?;
    let names = vec![name.to_string()];
    let mut note = String::new();
    match action {
        Action::Start | Action::Stop | Action::Restart | Action::Reload => {
            let method = match action {
                Action::Start => "StartUnit",
                Action::Stop => "StopUnit",
                Action::Restart => "RestartUnit",
                _ => "ReloadUnit",
            };
            let _job: OwnedObjectPath = call_auth(&m, method, &(name, "replace"))?;
        }
        Action::Enable => {
            let (has_install, _changes): (bool, Vec<(String, String, String)>) =
                call_auth(&m, "EnableUnitFiles", &(&names, false, false))?;
            if !has_install {
                note = " (it has no [Install] section, so nothing changed)".into();
            }
        }
        Action::Disable => {
            let _: Vec<(String, String, String)> =
                call_auth(&m, "DisableUnitFiles", &(&names, false))?;
        }
        Action::Mask => {
            let _: Vec<(String, String, String)> =
                call_auth(&m, "MaskUnitFiles", &(&names, false, false))?;
        }
        Action::Unmask => {
            let _: Vec<(String, String, String)> =
                call_auth(&m, "UnmaskUnitFiles", &(&names, false))?;
        }
        Action::ResetFailed => {
            let _: () = call_auth(&m, "ResetFailedUnit", &(name,))?;
        }
    }
    if matches!(
        action,
        Action::Enable | Action::Disable | Action::Mask | Action::Unmask
    ) {
        daemon_reload(bus)?;
    }
    Ok(format!("{} {name}{note}", action.past_tense()))
}

/// Blocks until the connection is lost, calling `on_change` whenever a unit starts,
/// stops or fails, a job finishes, unit files change, or systemd reloads.
/// With `changed_now`, also calls it once right after subscribing, to catch up on
/// changes missed while we weren't watching.
pub fn watch(bus: Bus, changed_now: bool, on_change: impl Fn()) -> Res<()> {
    let conn = bus.connection()?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path_namespace(MANAGER_PATH)
        .map_err(err)?
        .build();
    let signals =
        zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(256)).map_err(err)?;
    // systemd only emits most signals once a client has subscribed.
    let _: () = manager(bus)?.call("Subscribe", &()).map_err(err)?;
    if changed_now {
        on_change();
    }

    for msg in signals {
        // A broken connection shows up as an error; reconnecting is up to the caller.
        let Ok(msg) = msg else { break };
        let header = msg.header();
        let relevant = match header.member().map(|m| m.as_str()) {
            Some("JobRemoved" | "UnitFilesChanged" | "Reloading") => true,
            // Catches services that crash or exit on their own, without a job.
            Some("PropertiesChanged") => msg
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .is_ok_and(|(iface, changed, _)| {
                    iface == "org.freedesktop.systemd1.Unit" && changed.contains_key("ActiveState")
                }),
            _ => false,
        };
        if relevant {
            on_change();
        }
    }
    Err("lost connection to systemd".into())
}

/// Watches `bus` forever: like [`watch`], but reconnects when the connection drops,
/// waiting 1 s, then 2 s, ... up to 30 s between attempts.
pub fn watch_forever(bus: Bus, on_change: impl Fn()) {
    let mut delay = std::time::Duration::from_secs(1);
    let mut attempt = 0u32;
    let mut last_error = String::new();
    loop {
        let started = std::time::Instant::now();
        let res = watch(bus, attempt > 0, &on_change);
        attempt += 1;
        let e = res.err().unwrap_or_default();
        if e != last_error {
            eprintln!("watching {bus:?} services: {e}; reconnecting");
            last_error = e;
        }
        bus.reset_connection();
        if started.elapsed() > std::time::Duration::from_secs(30) {
            // We were connected for a while: start the back-off over.
            delay = std::time::Duration::from_secs(1);
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_secs(30));
    }
}

/// Where edits to a unit are saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditTarget {
    /// The unit file lives in the admin directory, so it is edited directly.
    File(PathBuf),
    /// A vendor unit (e.g. under /usr/lib). It is never modified in place; edits go
    /// into a drop-in that overrides only the settings it lists, the way
    /// `systemctl edit` does, so package updates to the unit still apply.
    DropIn(PathBuf),
}

impl EditTarget {
    pub fn path(&self) -> &Path {
        match self {
            EditTarget::File(p) | EditTarget::DropIn(p) => p,
        }
    }
}

pub fn edit_target(bus: Bus, name: &str, fragment: &str) -> EditTarget {
    let dir = bus.unit_dir();
    if !fragment.is_empty() && Path::new(fragment).starts_with(&dir) {
        EditTarget::File(PathBuf::from(fragment))
    } else {
        EditTarget::DropIn(dir.join(format!("{name}.d")).join("override.conf"))
    }
}

/// Local files that customize a vendor unit, i.e. what "Revert to vendor" removes:
/// our drop-in override, and a full copy in the admin dir that shadows a vendor unit
/// of the same name (older versions of this app saved edits that way).
pub fn local_overrides(bus: Bus, name: &str, fragment: &str) -> Vec<PathBuf> {
    overrides_in(
        &bus.unit_dir(),
        &bus.vendor_dirs(),
        name,
        fragment,
        |p: &Path| p.exists(),
    )
}

fn overrides_in(
    admin: &Path,
    vendor_dirs: &[PathBuf],
    name: &str,
    fragment: &str,
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let drop_in = admin.join(format!("{name}.d")).join("override.conf");
    if exists(&drop_in) {
        out.push(drop_in);
    }
    let copy = admin.join(name);
    let shadows_vendor = vendor_dirs.iter().any(|d| exists(&d.join(name)));
    if shadows_vendor && Path::new(fragment) == copy && exists(&copy) {
        out.push(copy);
    }
    out
}

/// Deletes files and directories, through pkexec for system units (unless we
/// already run as root).
pub fn remove_files(bus: Bus, paths: &[PathBuf]) -> Res<()> {
    if paths.is_empty() {
        return Ok(());
    }
    if bus == Bus::User || running_as_root() {
        for p in paths {
            let res = if p.is_dir() {
                std::fs::remove_dir_all(p)
            } else {
                std::fs::remove_file(p)
            };
            match res {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", p.display())),
            }
        }
        return Ok(());
    }
    let out = Command::new("pkexec")
        .args(["rm", "-rf", "--"])
        .args(paths)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run pkexec: {e}"))?;
    pkexec_result(&out)
}

/// Whether `delete_service` may remove this unit: only units created locally in the
/// admin directory, never vendor units or local copies that replace one.
pub fn is_deletable(bus: Bus, name: &str, fragment: &str) -> bool {
    !fragment.is_empty()
        && Path::new(fragment).starts_with(bus.unit_dir())
        && !local_overrides(bus, name, fragment)
            .iter()
            .any(|p| p == Path::new(fragment))
}

/// The timer with the same name as a locally created service (`foo.timer` for
/// `foo.service`), if there is one in the admin directory.
pub fn local_timer(bus: Bus, name: &str) -> Option<String> {
    let timer = format!("{}.timer", name.strip_suffix(".service")?);
    bus.unit_dir().join(&timer).exists().then_some(timer)
}

/// Files that make up a locally created unit: its file, its drop-in directory, and
/// its timer.
pub fn unit_files(bus: Bus, name: &str, fragment: &str) -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from(fragment)];
    let dir = bus.unit_dir();
    let drop_ins = dir.join(format!("{name}.d"));
    if drop_ins.exists() {
        files.push(drop_ins);
    }
    if let Some(timer) = local_timer(bus, name) {
        files.push(dir.join(timer));
    }
    files
}

/// Stops (if `running`) and disables a locally created service, deletes its files and
/// reloads systemd.
pub fn delete_service(bus: Bus, name: &str, fragment: &str, running: bool) -> Res<()> {
    if !is_deletable(bus, name, fragment) {
        return Err(format!("{name} is not a locally created unit"));
    }
    let m = manager(bus)?;
    let timer = local_timer(bus, name);
    if let Some(timer) = &timer {
        // Stop the timer first, so it can't start the service again.
        let _: Res<OwnedObjectPath> = call_auth(&m, "StopUnit", &(timer.as_str(), "replace"));
    }
    if running {
        run_action(bus, name, Action::Stop)?;
    }
    let mut names = vec![name.to_string()];
    names.extend(timer);
    // Fails harmlessly for units that have no [Install] section.
    let _: Res<Vec<(String, String, String)>> = call_auth(&m, "DisableUnitFiles", &(&names, false));
    remove_files(bus, &unit_files(bus, name, fragment))?;
    daemon_reload(bus)?;
    // Forget a failed state, so the unit disappears from the list.
    let _: Res<()> = call_auth(&m, "ResetFailedUnit", &(name,));
    Ok(())
}

fn pkexec_result(out: &std::process::Output) -> Res<()> {
    if out.status.success() {
        Ok(())
    } else if matches!(out.status.code(), Some(126) | Some(127)) {
        Err("Authorization was cancelled or denied".into())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn read_file(path: &str) -> Res<String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

/// Writes a unit file. System units are written through pkexec so polkit can authorize it
/// (unless we already run as root).
pub fn write_unit_file(bus: Bus, path: &Path, content: &str) -> Res<()> {
    if bus == Bus::User || running_as_root() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        return std::fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()));
    }
    // One pkexec call creates the parent directory (needed for new drop-ins) and
    // writes the file, so polkit asks only once.
    let mut child = Command::new("pkexec")
        .args([
            "/bin/sh",
            "-c",
            r#"mkdir -p -- "$(dirname -- "$1")" && cat > "$1""#,
            "sh",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run pkexec: {e}"))?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(content.as_bytes())
        .map_err(err)?;
    let out = child.wait_with_output().map_err(err)?;
    pkexec_result(&out)
}

fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

/// Validates a unit name, adding ".service" if it has no suffix.
pub fn normalize_name(input: &str) -> Res<String> {
    let mut name = input.trim().to_string();
    if name.is_empty() {
        return Err("Enter a name for the service".into());
    }
    if !name.ends_with(".service") {
        name.push_str(".service");
    }
    let stem = name.trim_end_matches(".service");
    let ok = !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ":-_.\\@".contains(c));
    if !ok {
        return Err("Names may only contain letters, digits and : - _ . \\ @".into());
    }
    Ok(name)
}

/// Directories systemd searches for commands that aren't given as absolute paths.
const COMMAND_PATH: [&str; 6] = [
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// Checks the program of an ExecStart=/ExecStop= line and returns the line with
/// the program as an absolute path. Keeps systemd's prefixes (`-`, `@`, `+`, ...).
pub fn resolve_command(line: &str) -> Res<String> {
    let line = line.trim();
    let body = line.trim_start_matches(['-', '@', ':', '+', '!']);
    let prefix = &line[..line.len() - body.len()];
    let (program, rest) = match body.split_once(char::is_whitespace) {
        Some((p, r)) => (p, format!(" {}", r.trim_start())),
        None => (body, String::new()),
    };
    if program.is_empty() {
        return Err("Enter the command the service should run".into());
    }
    let path = if program.starts_with('/') {
        PathBuf::from(program)
    } else if program.contains('/') {
        return Err(format!(
            "Use an absolute path for {program}; systemd doesn't run commands from a working directory"
        ));
    } else {
        COMMAND_PATH
            .iter()
            .map(|d| Path::new(d).join(program))
            .find(|p| is_executable(p))
            .ok_or_else(|| format!("Command not found: {program}"))?
    };
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    if !is_executable(&path) {
        return Err(format!("{} is not an executable file", path.display()));
    }
    Ok(format!("{prefix}{}{rest}", path.display()))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Parses "KEY=value OTHER=value" into separate assignments.
pub fn parse_environment(input: &str) -> Res<Vec<String>> {
    input
        .split_whitespace()
        .map(|pair| {
            let (key, _) = pair
                .split_once('=')
                .ok_or_else(|| format!("{pair}: environment variables are written KEY=value"))?;
            let valid = key
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return Err(format!("{key} is not a valid variable name"));
            }
            Ok(pair.to_string())
        })
        .collect()
}

pub struct NewService {
    pub name: String,
    pub description: String,
    pub exec_start: String,
    /// Optional ExecStop= line.
    pub exec_stop: String,
    /// KEY=value assignments.
    pub environment: Vec<String>,
    /// Seconds to wait before restarting (0 = systemd's default).
    pub restart_sec: u32,
    pub service_type: String,
    pub restart: String,
    pub working_dir: String,
    pub user: String,
    pub wanted_by: String,
    /// OnCalendar= expression. When set, a timer starts the service on that schedule
    /// and the service itself gets no [Install] section.
    pub schedule: Option<String>,
}

pub fn render_unit(bus: Bus, s: &NewService) -> String {
    let mut out = String::from("[Unit]\n");
    if !s.description.is_empty() {
        out += &format!("Description={}\n", s.description);
    }
    if bus == Bus::System {
        out += "After=network.target\n";
    }
    out += "\n[Service]\n";
    out += &format!("Type={}\n", s.service_type);
    out += &format!("ExecStart={}\n", s.exec_start);
    if !s.exec_stop.is_empty() {
        out += &format!("ExecStop={}\n", s.exec_stop);
    }
    for env in &s.environment {
        out += &format!("Environment=\"{env}\"\n");
    }
    if !s.working_dir.is_empty() {
        out += &format!("WorkingDirectory={}\n", s.working_dir);
    }
    if bus == Bus::System && !s.user.is_empty() {
        out += &format!("User={}\n", s.user);
    }
    out += &format!("Restart={}\n", s.restart);
    if s.restart != "no" && s.restart_sec > 0 {
        out += &format!("RestartSec={}\n", s.restart_sec);
    }
    if !s.wanted_by.is_empty() && s.schedule.is_none() {
        out += &format!("\n[Install]\nWantedBy={}\n", s.wanted_by);
    }
    out
}

/// The timer unit that runs a new service on `s.schedule`.
pub fn render_timer(s: &NewService) -> Option<String> {
    let schedule = s.schedule.as_ref()?;
    Some(format!(
        "[Unit]\n\
         Description=Run {} on a schedule\n\
         \n\
         [Timer]\n\
         OnCalendar={schedule}\n\
         # Run once at startup if the computer was off at the scheduled time.\n\
         Persistent=true\n\
         Unit={}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        s.name, s.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_get_service_suffix() {
        assert_eq!(normalize_name(" my-app ").unwrap(), "my-app.service");
        assert_eq!(normalize_name("a@b.service").unwrap(), "a@b.service");
        assert!(normalize_name("").is_err());
        assert!(normalize_name("../evil").is_err());
        assert!(normalize_name("has space").is_err());
    }

    #[test]
    fn renders_unit() {
        let spec = NewService {
            name: "demo.service".into(),
            description: "Demo".into(),
            exec_start: "/usr/bin/demo --flag".into(),
            exec_stop: String::new(),
            environment: vec!["PORT=8080".into()],
            restart_sec: 5,
            service_type: "simple".into(),
            restart: "on-failure".into(),
            working_dir: String::new(),
            user: "nobody".into(),
            wanted_by: "multi-user.target".into(),
            schedule: None,
        };
        let system = render_unit(Bus::System, &spec);
        assert!(system.contains("ExecStart=/usr/bin/demo --flag\n"));
        assert!(system.contains("User=nobody\n"));
        assert!(system.contains("Environment=\"PORT=8080\"\n"));
        assert!(system.contains("RestartSec=5\n"));
        assert!(!system.contains("ExecStop="));
        assert!(system.ends_with("[Install]\nWantedBy=multi-user.target\n"));
        assert!(!render_unit(Bus::User, &spec).contains("User="));
        assert!(render_timer(&spec).is_none());

        let scheduled = NewService {
            schedule: Some("daily".into()),
            ..spec
        };
        assert!(!render_unit(Bus::System, &scheduled).contains("[Install]"));
        let timer = render_timer(&scheduled).unwrap();
        assert!(timer.contains("OnCalendar=daily\n"));
        assert!(timer.contains("Unit=demo.service\n"));
        assert!(timer.ends_with("WantedBy=timers.target\n"));
    }

    #[test]
    fn resolves_commands() {
        let sh = resolve_command("sh -c 'echo hi'").unwrap();
        assert!(
            sh.starts_with('/') && sh.ends_with("/sh -c 'echo hi'"),
            "{sh}"
        );
        assert!(resolve_command("-sh").unwrap().starts_with("-/"));
        assert!(resolve_command("/no/such/program").is_err());
        assert!(resolve_command("bin/relative").is_err());
        assert!(resolve_command("definitely-not-a-command-xyz").is_err());
    }

    #[test]
    fn parses_environment() {
        assert_eq!(
            parse_environment(" A=1  B_2=x=y ").unwrap(),
            vec!["A=1", "B_2=x=y"]
        );
        assert!(parse_environment("NOEQUALS").is_err());
        assert!(parse_environment("1A=x").is_err());
        assert!(parse_environment("").unwrap().is_empty());
    }

    #[test]
    fn risky_actions_need_confirmation() {
        let ask = |bus, name, action, running| confirmation(bus, name, action, running).is_some();
        assert!(ask(Bus::System, "sshd.service", Action::Stop, true));
        assert!(ask(
            Bus::System,
            "NetworkManager.service",
            Action::Disable,
            false
        ));
        assert!(ask(Bus::System, "gdm.service", Action::Restart, true));
        assert!(ask(Bus::System, "foo.service", Action::Mask, false));
        // Ordinary services, stopped services, safe actions and user services go ahead.
        assert!(!ask(Bus::System, "foo.service", Action::Stop, true));
        assert!(!ask(Bus::System, "sshd.service", Action::Stop, false));
        assert!(!ask(Bus::System, "sshd.service", Action::Start, false));
        assert!(!ask(Bus::User, "sshd.service", Action::Mask, true));
    }

    #[test]
    fn describes_exits_and_cpu() {
        assert_eq!(describe_exit(1, 3).as_deref(), Some("exit code 3"));
        assert_eq!(describe_exit(2, 9).as_deref(), Some("killed by SIGKILL"));
        assert_eq!(
            describe_exit(3, 11).as_deref(),
            Some("crashed with SIGSEGV (core dumped)")
        );
        assert_eq!(describe_exit(0, 0), None);
        assert_eq!(format_cpu(250_000_000), "0.25 s");
        assert_eq!(format_cpu(184_000_000_000), "3 min 4 s");
        assert_eq!(format_cpu(7_500_000_000_000), "2 h 5 min");
        assert_eq!(describe_result("success"), "Success");
        assert!(describe_result("weird").contains("weird"));
    }

    #[test]
    fn only_local_units_are_deletable() {
        assert!(!is_deletable(Bus::System, "a.service", ""));
        assert!(!is_deletable(
            Bus::System,
            "a.service",
            "/usr/lib/systemd/system/a.service"
        ));
    }

    #[test]
    fn vendor_units_get_drop_in() {
        assert_eq!(
            edit_target(
                Bus::System,
                "a.service",
                "/usr/lib/systemd/system/a.service"
            ),
            EditTarget::DropIn(PathBuf::from(
                "/etc/systemd/system/a.service.d/override.conf"
            ))
        );
        assert_eq!(
            edit_target(Bus::System, "a.service", "/etc/systemd/system/a.service"),
            EditTarget::File(PathBuf::from("/etc/systemd/system/a.service"))
        );
    }

    #[test]
    fn finds_local_overrides() {
        let admin = Path::new("/etc/systemd/system");
        let vendor = [PathBuf::from("/usr/lib/systemd/system")];
        let present = |files: &'static [&'static str]| {
            move |p: &Path| files.iter().any(|f| p == Path::new(f))
        };

        // A drop-in on a vendor unit.
        let files = &[
            "/etc/systemd/system/a.service.d/override.conf",
            "/usr/lib/systemd/system/a.service",
        ];
        assert_eq!(
            overrides_in(
                admin,
                &vendor,
                "a.service",
                "/usr/lib/systemd/system/a.service",
                present(files)
            ),
            vec![PathBuf::from(
                "/etc/systemd/system/a.service.d/override.conf"
            )]
        );
        // A full copy shadowing a vendor unit.
        let files = &[
            "/etc/systemd/system/a.service",
            "/usr/lib/systemd/system/a.service",
        ];
        assert_eq!(
            overrides_in(
                admin,
                &vendor,
                "a.service",
                "/etc/systemd/system/a.service",
                present(files)
            ),
            vec![PathBuf::from("/etc/systemd/system/a.service")]
        );
        // A unit the admin created: nothing to revert to.
        let files = &["/etc/systemd/system/a.service"];
        assert!(overrides_in(
            admin,
            &vendor,
            "a.service",
            "/etc/systemd/system/a.service",
            present(files)
        )
        .is_empty());
    }
}
