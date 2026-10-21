//! CAN for the simulator: an emulated slcan adapter, so a project's
//! simulator can sit behind "a CAN adapter" on a serial port (or an
//! in-process loopback) and be reached with the real slcan and ISO-TP code.

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pb_core::clock::SystemClock;
use pb_core::config::{ConnectionProfile, TransportKind};
use pb_core::error::CoreError;
use pb_core::project::Project;
use pb_core::transport::can::slcan::{format_frame, parse_frame};
use pb_core::transport::can::{CanBus, CanFrame, CanOptions, CanTransport};
use pb_core::transport::Transport;

use crate::Simulator;

/// The device side of an slcan adapter: it answers the host's slcan
/// commands on `link` and turns frame lines into CAN frames (and back).
pub struct EmulatedAdapter {
    link: Box<dyn Transport>,
    buf: Vec<u8>,
    open: bool,
}

impl EmulatedAdapter {
    /// An adapter on a byte link (a serial port, a loopback end).
    pub fn new(link: Box<dyn Transport>) -> EmulatedAdapter {
        EmulatedAdapter {
            link,
            buf: Vec::new(),
            open: false,
        }
    }

    fn reply(&mut self, s: &[u8]) -> io::Result<()> {
        self.link.write_all(s)
    }
}

impl CanBus for EmulatedAdapter {
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<CanFrame>> {
        let end = Instant::now() + timeout;
        let mut tmp = [0u8; 4096];
        loop {
            while let Some(i) = self.buf.iter().position(|&b| b == b'\r') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                match line.chars().next() {
                    None => {}
                    Some('O') => {
                        self.open = true;
                        self.reply(b"\r")?;
                    }
                    Some('C') => {
                        self.open = false;
                        self.reply(b"\r")?;
                    }
                    Some('t' | 'T' | 'd' | 'D' | 'b' | 'B') => match parse_frame(&line) {
                        Ok(Some(f)) if self.open => {
                            // Lawicel adapters acknowledge t/T with z/Z.
                            let ack: &[u8] = match line.as_bytes()[0] {
                                b't' => b"z\r",
                                b'T' => b"Z\r",
                                _ => b"\r",
                            };
                            self.reply(ack)?;
                            return Ok(Some(f));
                        }
                        _ => self.reply(b"\x07")?,
                    },
                    // Bit rates, version, filters, timestamps: accepted.
                    Some(_) => self.reply(b"\r")?,
                }
            }
            let n = self.link.read(&mut tmp)?;
            self.buf.extend_from_slice(&tmp[..n]);
            if n == 0 && Instant::now() >= end {
                return Ok(None);
            }
        }
    }

    fn send(&mut self, f: &CanFrame) -> io::Result<()> {
        if !self.open {
            return Ok(()); // a closed adapter puts nothing on the bus
        }
        let mut s = format_frame(f);
        s.push('\r');
        self.link.write_all(s.as_bytes())
    }

    fn describe(&self) -> String {
        format!("emulated slcan adapter on {}", self.link.describe())
    }
}

/// The first CAN profile of a project, or the one named `name`.
pub fn can_profile(project: &Project, name: Option<&str>) -> Result<ConnectionProfile, CoreError> {
    let profiles = &project.config.connection.profiles;
    let p = match name {
        Some(n) => profiles.iter().find(|p| p.name == n),
        None => profiles.iter().find(|p| p.transport == TransportKind::Can),
    };
    match p {
        Some(p) if p.transport == TransportKind::Can => Ok(p.clone()),
        Some(p) => Err(CoreError::Config(format!(
            "profile `{}` is not a can profile",
            p.name
        ))),
        None => Err(CoreError::Config(
            "the project has no can connection profile".into(),
        )),
    }
}

/// Runs the project's simulator behind an emulated slcan adapter on `link`,
/// with the identifiers of the host profile `host` swapped round, until
/// `stop`.
pub fn spawn_slcan_device(
    project: &Project,
    host: &ConnectionProfile,
    link: Box<dyn Transport>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, CoreError> {
    let o = CanOptions::from_profile(host)?.peer();
    let sim = Simulator::for_project(project, SystemClock::shared())?;
    let t = CanTransport::new(Box::new(EmulatedAdapter::new(link)), o);
    std::thread::Builder::new()
        .name("protobench-sim-can".into())
        .spawn(move || sim.run(Box::new(t), stop))
        .map_err(CoreError::IoBare)
}
