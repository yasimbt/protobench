//! SEGGER RTT through a debug probe: the firmware's RTT buffers in target
//! RAM, read and written by the probe while the core runs.
//!
//! The data channel (`rtt_up`/`rtt_down`) is a byte stream like a UART
//! (use a framer such as COBS); an optional text channel
//! (`rtt_text_channel`, usually 0 with `printf`) goes to the text console.
//! The probe side sits behind [`RttTarget`], so the transport is tested
//! with a mock; [`probe::ProbeRsTarget`] (cargo feature `rtt`) drives J-Link,
//! ST-Link and CMSIS-DAP probes through probe-rs.

#[cfg(feature = "rtt")]
pub mod probe;

use std::io;
use std::time::{Duration, Instant};

use super::{Transport, WRITE_TIMEOUT};
use crate::config::ConnectionProfile;
use crate::error::CoreError;

/// Longest text line kept before it is cut.
pub const MAX_LINE: usize = 4096;

/// The target side of RTT.
pub trait RttTarget: Send {
    /// Reads what the target wrote to up channel `channel`.
    fn read_up(&mut self, channel: usize, buf: &mut [u8]) -> io::Result<usize>;
    /// Writes to down channel `channel`; returns how much fit.
    fn write_down(&mut self, channel: usize, data: &[u8]) -> io::Result<usize>;
    /// Short description (probe and chip).
    fn describe(&self) -> String;
}

/// RTT settings of a profile.
#[derive(Debug, Clone, PartialEq)]
pub struct RttOptions {
    /// probe-rs target name, e.g. `STM32F411RETx`.
    pub chip: String,
    /// Probe selector: `VID:PID`, `VID:PID:SERIAL` or a serial number.
    pub probe: Option<String>,
    /// Up channel carrying data.
    pub up: usize,
    /// Down channel carrying data.
    pub down: usize,
    /// Up channel shown as text.
    pub text: Option<usize>,
    /// Attach to the running core (true) or reset it while attaching.
    pub attach: bool,
    /// ELF file whose `_SEGGER_RTT` symbol gives the control block address.
    pub elf: Option<std::path::PathBuf>,
    /// SWD/JTAG clock in kHz.
    pub speed_khz: Option<u32>,
}

impl RttOptions {
    /// The options of a profile; `elf` relative to `project_dir`.
    pub fn from_profile(p: &ConnectionProfile, project_dir: &std::path::Path) -> Self {
        RttOptions {
            chip: p.chip.trim().to_string(),
            probe: p.probe.clone().filter(|s| !s.trim().is_empty()),
            up: p.rtt_up,
            down: p.rtt_down,
            text: p.rtt_text_channel,
            attach: p.attach,
            elf: p.elf.as_ref().map(|e| project_dir.join(e)),
            speed_khz: p.probe_speed_khz,
        }
    }
}

/// A debug probe found on the system.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProbeInfo {
    /// Probe name, e.g. `J-Link` or `STLink V3`.
    pub identifier: String,
    /// USB vendor ID.
    pub vid: u16,
    /// USB product ID.
    pub pid: u16,
    /// Serial number, when the probe reports one.
    pub serial_number: Option<String>,
}

impl ProbeInfo {
    /// `VID:PID[:SERIAL]`, the form `probe = …` takes.
    pub fn selector(&self) -> String {
        match &self.serial_number {
            Some(s) => format!("{:04x}:{:04x}:{s}", self.vid, self.pid),
            None => format!("{:04x}:{:04x}", self.vid, self.pid),
        }
    }
}

/// Picks the probe for `want` (`VID:PID`, `VID:PID:SERIAL` or a serial
/// number); without a selector the only probe attached. Several matches
/// or none are errors that list what is there.
pub fn select_probe<'a>(
    probes: &'a [ProbeInfo],
    want: Option<&str>,
) -> Result<&'a ProbeInfo, String> {
    let list = || {
        if probes.is_empty() {
            "no debug probes are attached".to_string()
        } else {
            format!(
                "attached: {}",
                probes
                    .iter()
                    .map(|p| format!("{} ({})", p.identifier, p.selector()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    let hex = |s: &str| u16::from_str_radix(s.trim(), 16).ok();
    let matches: Vec<&ProbeInfo> = match want.map(str::trim) {
        None | Some("") => probes.iter().collect(),
        Some(w) => {
            let parts: Vec<&str> = w.split(':').collect();
            probes
                .iter()
                .filter(|p| match parts.as_slice() {
                    [v, d] if hex(v).is_some() && hex(d).is_some() => {
                        hex(v) == Some(p.vid) && hex(d) == Some(p.pid)
                    }
                    [v, d, s] => {
                        hex(v) == Some(p.vid)
                            && hex(d) == Some(p.pid)
                            && p.serial_number
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(s))
                    }
                    _ => p
                        .serial_number
                        .as_deref()
                        .is_some_and(|n| n.eq_ignore_ascii_case(w)),
                })
                .collect()
        }
    };
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(match want {
            Some(w) => format!("no debug probe matches `{w}`; {}", list()),
            None => list(),
        }),
        _ => Err(format!(
            "{} debug probes match{}; set `probe` to one of them ({})",
            matches.len(),
            want.map(|w| format!(" `{w}`")).unwrap_or_default(),
            list()
        )),
    }
}

/// An RTT link.
pub struct RttTransport {
    target: Box<dyn RttTarget>,
    up: usize,
    down: usize,
    text: Option<usize>,
    text_buf: Vec<u8>,
    lines: Vec<String>,
}

impl RttTransport {
    /// RTT over `target` with the channels in `o`.
    pub fn new(target: Box<dyn RttTarget>, o: &RttOptions) -> RttTransport {
        RttTransport {
            target,
            up: o.up,
            down: o.down,
            text: o.text.filter(|t| *t != o.up),
            text_buf: Vec::new(),
            lines: Vec::new(),
        }
    }

    fn poll_text(&mut self) -> io::Result<bool> {
        let Some(ch) = self.text else {
            return Ok(false);
        };
        let mut tmp = [0u8; 1024];
        let n = self.target.read_up(ch, &mut tmp)?;
        self.text_buf.extend_from_slice(&tmp[..n]);
        while let Some(i) = self.text_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.text_buf.drain(..=i).collect();
            let s = String::from_utf8_lossy(&line[..line.len() - 1]);
            self.lines.push(s.trim_end_matches('\r').to_string());
        }
        if self.text_buf.len() > MAX_LINE {
            let s = String::from_utf8_lossy(&self.text_buf).into_owned();
            self.text_buf.clear();
            self.lines.push(s);
        }
        Ok(n > 0)
    }
}

impl Transport for RttTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let text = self.poll_text()?;
        let n = self.target.read_up(self.up, buf)?;
        if n == 0 && !text {
            // Every poll is a USB round trip: do not spin.
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(n)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let end = Instant::now() + WRITE_TIMEOUT;
        let mut off = 0;
        while off < data.len() {
            off += self.target.write_down(self.down, &data[off..])?;
            if off < data.len() {
                if Instant::now() >= end {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "RTT down channel {} full: wrote {off} of {} bytes (is the firmware reading it?)",
                            self.down,
                            data.len()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!(
            "{} (RTT up {} down {}{})",
            self.target.describe(),
            self.up,
            self.down,
            self.text.map(|t| format!(", text {t}")).unwrap_or_default()
        )
    }

    fn take_text(&mut self) -> Vec<String> {
        std::mem::take(&mut self.lines)
    }
}

/// Debug probes attached to this machine (empty without the `rtt`
/// feature).
pub fn list_probes() -> Vec<ProbeInfo> {
    #[cfg(feature = "rtt")]
    {
        probe::list()
    }
    #[cfg(not(feature = "rtt"))]
    {
        Vec::new()
    }
}

/// True if this build can talk to debug probes.
pub const SUPPORTED: bool = cfg!(feature = "rtt");

/// A connector for an RTT profile.
pub fn connector(
    p: &ConnectionProfile,
    project_dir: &std::path::Path,
) -> Result<super::Connector, CoreError> {
    let o = RttOptions::from_profile(p, project_dir);
    if o.chip.is_empty() {
        return Err(CoreError::Transport(
            "an rtt profile needs `chip` (e.g. STM32F411RETx)".into(),
        ));
    }
    #[cfg(feature = "rtt")]
    {
        Ok(Box::new(move || {
            let t = probe::ProbeRsTarget::open(&o)?;
            Ok(Box::new(RttTransport::new(Box::new(t), &o)) as Box<dyn Transport>)
        }))
    }
    #[cfg(not(feature = "rtt"))]
    {
        let _ = o;
        Err(CoreError::Transport(
            "this build has no RTT support (cargo feature `rtt`)".into(),
        ))
    }
}

#[cfg(test)]
mod tests;
