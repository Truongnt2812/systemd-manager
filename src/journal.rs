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

pub fn spawn(
    bus: Bus,
    unit: &str,
    lines: u32,
    follow: bool,
) -> std::io::Result<(JournalStream, async_channel::Receiver<String>)> {
    let mut cmd = Command::new("journalctl");
    if bus == Bus::User {
        cmd.arg("--user");
    }
    cmd.args(["--no-pager", "--no-hostname", "-o", "short-iso", "-n"])
        .arg(lines.to_string())
        .arg("-u")
        .arg(unit);
    if follow {
        cmd.arg("-f");
    }
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
