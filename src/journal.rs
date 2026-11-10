//! Streams `journalctl` output for one unit.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};

use crate::systemd::Bus;

/// A running journalctl process. Dropping it kills the process.
pub struct JournalStream {
    child: Child,
}

impl Drop for JournalStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Which journal entries to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Lowest priority to show, as journalctl names it ("err", "warning", ...).
    pub priority: Option<&'static str>,
    /// Only entries from the current boot.
    pub this_boot: bool,
    /// Only entries newer than this, as a relative time ("-1h", "-1d", ...).
    pub since: Option<&'static str>,
}

impl Filter {
    /// How many settings narrow the log.
    pub fn active_count(&self) -> usize {
        usize::from(self.priority.is_some())
            + usize::from(self.this_boot)
            + usize::from(self.since.is_some())
    }
}

/// The journalctl command line (without the program name).
pub fn args(bus: Bus, unit: &str, lines: u32, follow: bool, filter: Filter) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if bus == Bus::User {
        args.push("--user".into());
    }
    for a in ["--no-pager", "--no-hostname", "-o", "short-iso", "-n"] {
        args.push(a.into());
    }
    args.push(lines.to_string());
    if let Some(p) = filter.priority {
        args.push(format!("--priority={p}"));
    }
    if filter.this_boot {
        args.push("--boot".into());
    }
    if let Some(since) = filter.since {
        args.push(format!("--since={since}"));
    }
    args.push("-u".into());
    args.push(unit.into());
    if follow {
        args.push("-f".into());
    }
    args
}

pub fn spawn(
    bus: Bus,
    unit: &str,
    lines: u32,
    follow: bool,
    filter: Filter,
) -> std::io::Result<(JournalStream, async_channel::Receiver<String>)> {
    let mut cmd = Command::new("journalctl");
    cmd.args(args(bus, unit, lines, follow, filter));
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let (tx, rx) = async_channel::unbounded();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    pump(stdout, tx.clone());
    pump(stderr, tx);
    Ok((JournalStream { child }, rx))
}

fn pump(pipe: impl Read + Send + 'static, tx: async_channel::Sender<String>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx
                        .send_blocking(String::from_utf8_lossy(&buf).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_journalctl_args() {
        let plain = args(Bus::System, "a.service", 500, true, Filter::default());
        assert_eq!(
            plain.join(" "),
            "--no-pager --no-hostname -o short-iso -n 500 -u a.service -f"
        );
        let filter = Filter {
            priority: Some("err"),
            this_boot: true,
            since: Some("-1h"),
        };
        assert_eq!(filter.active_count(), 3);
        let filtered = args(Bus::User, "a.service", 100, false, filter);
        assert_eq!(
            filtered.join(" "),
            "--user --no-pager --no-hostname -o short-iso -n 100 --priority=err --boot --since=-1h -u a.service"
        );
    }
}
