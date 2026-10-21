//! RTT through probe-rs: J-Link, ST-Link and CMSIS-DAP (v2) probes.

use std::io;
use std::time::{Duration, Instant};

use probe_rs::probe::list::Lister;
use probe_rs::rtt::{Rtt, ScanRegion};
use probe_rs::{Permissions, Session};

use super::{select_probe, ProbeInfo, RttOptions, RttTarget};
use crate::error::CoreError;

/// How long to look for the RTT control block (the firmware may still be
/// starting).
pub const FIND_TIMEOUT: Duration = Duration::from_secs(2);

/// Probes attached to this machine.
pub fn list() -> Vec<ProbeInfo> {
    Lister::new()
        .list_all()
        .into_iter()
        .map(|p| ProbeInfo {
            identifier: p.identifier.clone(),
            vid: p.vendor_id,
            pid: p.product_id,
            serial_number: p.serial_number.clone(),
        })
        .collect()
}

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Reads the address of `_SEGGER_RTT` from an ELF file.
pub fn rtt_symbol(path: &std::path::Path) -> Result<u64, CoreError> {
    use object::{Object, ObjectSymbol};
    let data = std::fs::read(path).map_err(|e| CoreError::io(path, e))?;
    let file = object::File::parse(&*data)
        .map_err(|e| CoreError::Transport(format!("{}: {e}", path.display())))?;
    file.symbols()
        .find(|s| s.name() == Ok("_SEGGER_RTT"))
        .map(|s| s.address())
        .ok_or_else(|| {
            CoreError::Transport(format!(
                "{} has no `_SEGGER_RTT` symbol (is RTT linked in?)",
                path.display()
            ))
        })
}

/// A target reached through a probe.
pub struct ProbeRsTarget {
    session: Session,
    rtt: Rtt,
    name: String,
}

impl ProbeRsTarget {
    /// Opens the probe, attaches to the chip and finds the RTT control
    /// block (at the ELF's `_SEGGER_RTT`, or by scanning RAM).
    pub fn open(o: &RttOptions) -> Result<ProbeRsTarget, CoreError> {
        let t = |m: String| CoreError::Transport(format!("RTT: {m}"));
        let infos = Lister::new().list_all();
        let summary: Vec<ProbeInfo> = list();
        let chosen = select_probe(&summary, o.probe.as_deref()).map_err(t)?;
        let info = infos
            .iter()
            .find(|i| {
                i.vendor_id == chosen.vid
                    && i.product_id == chosen.pid
                    && i.serial_number == chosen.serial_number
            })
            .ok_or_else(|| t("the probe went away".into()))?;
        let mut probe = info.open().map_err(|e| t(e.to_string()))?;
        if let Some(khz) = o.speed_khz {
            probe.set_speed(khz).map_err(|e| t(e.to_string()))?;
        }
        let mut session = if o.attach {
            probe.attach(o.chip.as_str(), Permissions::default())
        } else {
            probe.attach_under_reset(o.chip.as_str(), Permissions::default())
        }
        .map_err(|e| t(format!("{}: {e}", o.chip)))?;
        if !o.attach {
            // Start the firmware from the top, so RTT starts fresh too.
            session
                .core(0)
                .and_then(|mut c| c.reset())
                .map_err(|e| t(e.to_string()))?;
        }
        let region = match &o.elf {
            Some(elf) => ScanRegion::Exact(rtt_symbol(elf)?),
            None => ScanRegion::Ram,
        };
        let end = Instant::now() + FIND_TIMEOUT;
        let rtt = loop {
            let mut core = session.core(0).map_err(|e| t(e.to_string()))?;
            match Rtt::attach_region(&mut core, &region) {
                Ok(r) => break r,
                Err(_) if Instant::now() < end => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    return Err(t(format!(
                    "no RTT control block found ({e}); is SEGGER RTT in the firmware and running?"
                )))
                }
            }
        };
        let mut me = ProbeRsTarget {
            session,
            rtt,
            name: format!("{} on {}", o.chip, chosen.identifier),
        };
        me.check_channels(o).map_err(t)?;
        Ok(me)
    }

    /// The channels the profile names must exist; the error lists those
    /// that do.
    fn check_channels(&mut self, o: &RttOptions) -> Result<(), String> {
        let ups: Vec<String> = self
            .rtt
            .up_channels()
            .iter()
            .map(|c| format!("{} {:?}", c.number(), c.name().unwrap_or("")))
            .collect();
        let downs: Vec<String> = self
            .rtt
            .down_channels()
            .iter()
            .map(|c| format!("{} {:?}", c.number(), c.name().unwrap_or("")))
            .collect();
        let have = || {
            format!(
                "the firmware has up {} and down {}",
                ups.join(", "),
                downs.join(", ")
            )
        };
        for (what, ch) in [("rtt_up", Some(o.up)), ("rtt_text_channel", o.text)] {
            if let Some(ch) = ch {
                if self.rtt.up_channel(ch).is_none() {
                    return Err(format!("{what}: no up channel {ch}; {}", have()));
                }
            }
        }
        if self.rtt.down_channel(o.down).is_none() {
            return Err(format!("rtt_down: no down channel {}; {}", o.down, have()));
        }
        Ok(())
    }
}

impl RttTarget for ProbeRsTarget {
    fn read_up(&mut self, channel: usize, buf: &mut [u8]) -> io::Result<usize> {
        let mut core = self.session.core(0).map_err(other)?;
        let ch = self
            .rtt
            .up_channel(channel)
            .ok_or_else(|| other(format!("no up channel {channel}")))?;
        ch.read(&mut core, buf).map_err(other)
    }

    fn write_down(&mut self, channel: usize, data: &[u8]) -> io::Result<usize> {
        let mut core = self.session.core(0).map_err(other)?;
        let ch = self
            .rtt
            .down_channel(channel)
            .ok_or_else(|| other(format!("no down channel {channel}")))?;
        ch.write(&mut core, data).map_err(other)
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}
