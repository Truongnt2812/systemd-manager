//! Thin blocking wrapper around the systemd D-Bus API (org.freedesktop.systemd1).
//!
//! Every function here blocks, so the UI calls them through `gio::spawn_blocking`.
//! Privileged calls on the system bus set the "allow interactive authorization"
//! flag, which lets systemd ask polkit to show a password prompt.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

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
    fn connection(self) -> Res<Connection> {
        static SYSTEM: OnceLock<Connection> = OnceLock::new();
        static USER: OnceLock<Connection> = OnceLock::new();
        let cell = match self {
            Bus::System => &SYSTEM,
            Bus::User => &USER,
        };
        if let Some(c) = cell.get() {
            return Ok(c.clone());
        }
        let c = match self {
            Bus::System => Connection::system(),
            Bus::User => Connection::session(),
        }
        .map_err(err)?;
        Ok(cell.get_or_init(|| c).clone())
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
    pub main_pid: u32,
    pub memory: Option<u64>,
    /// Microseconds since the epoch at which the unit last entered the active state.
    pub active_since: u64,
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
        }
    }
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
    let memory = svc
        .get_property::<u64>("MemoryCurrent")
        .ok()
        .filter(|m| *m != u64::MAX);
    Ok(Details {
        description: s("Description"),
        load_state: s("LoadState"),
        active_state: s("ActiveState"),
        sub_state: s("SubState"),
        file_state: s("UnitFileState"),
        fragment_path: s("FragmentPath"),
        main_pid: svc.get_property::<u32>("MainPID").unwrap_or(0),
        memory,
        active_since: unit
            .get_property::<u64>("ActiveEnterTimestamp")
            .unwrap_or(0),
    })
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
    }
    if matches!(
        action,
        Action::Enable | Action::Disable | Action::Mask | Action::Unmask
    ) {
        daemon_reload(bus)?;
    }
    Ok(format!("{} {name}{note}", action.past_tense()))
}

/// Blocks forever, calling `on_change` whenever a unit starts, stops or fails,
/// a job finishes, unit files change, or systemd reloads.
pub fn watch(bus: Bus, on_change: impl Fn()) -> Res<()> {
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

    for msg in signals.flatten() {
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

/// Where an edited copy of a unit should be saved. Vendor units (e.g. under
/// /usr/lib) are never modified in place; an override copy goes to the admin dir.
pub fn editable_path(bus: Bus, name: &str, fragment: &str) -> PathBuf {
    let dir = bus.unit_dir();
    if !fragment.is_empty() && Path::new(fragment).starts_with(&dir) {
        PathBuf::from(fragment)
    } else {
        dir.join(name)
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
    let mut child = Command::new("pkexec")
        .arg("tee")
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
    if out.status.success() {
        Ok(())
    } else if matches!(out.status.code(), Some(126) | Some(127)) {
        Err("Authorization was cancelled or denied".into())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
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

pub struct NewService {
    pub description: String,
    pub exec_start: String,
    pub service_type: String,
    pub restart: String,
    pub working_dir: String,
    pub user: String,
    pub wanted_by: String,
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
    if !s.working_dir.is_empty() {
        out += &format!("WorkingDirectory={}\n", s.working_dir);
    }
    if bus == Bus::System && !s.user.is_empty() {
        out += &format!("User={}\n", s.user);
    }
    out += &format!("Restart={}\n", s.restart);
    if !s.wanted_by.is_empty() {
        out += &format!("\n[Install]\nWantedBy={}\n", s.wanted_by);
    }
    out
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
            description: "Demo".into(),
            exec_start: "/usr/bin/demo --flag".into(),
            service_type: "simple".into(),
            restart: "on-failure".into(),
            working_dir: String::new(),
            user: "nobody".into(),
            wanted_by: "multi-user.target".into(),
        };
        let system = render_unit(Bus::System, &spec);
        assert!(system.contains("ExecStart=/usr/bin/demo --flag\n"));
        assert!(system.contains("User=nobody\n"));
        assert!(system.ends_with("[Install]\nWantedBy=multi-user.target\n"));
        assert!(!render_unit(Bus::User, &spec).contains("User="));
    }

    #[test]
    fn vendor_units_get_override_path() {
        assert_eq!(
            editable_path(
                Bus::System,
                "a.service",
                "/usr/lib/systemd/system/a.service"
            ),
            PathBuf::from("/etc/systemd/system/a.service")
        );
        assert_eq!(
            editable_path(Bus::System, "a.service", "/etc/systemd/system/a.service"),
            PathBuf::from("/etc/systemd/system/a.service")
        );
    }
}
