//! Serial / USB-CDC transport.

use std::io::{self, Read, Write};
use std::time::Duration;

use super::{Transport, READ_TIMEOUT, WRITE_TIMEOUT};
use crate::config::{ConnectionProfile, FlowControl, Parity};
use crate::error::CoreError;

/// A serial port found on the system.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PortInfo {
    /// Port name (`COM5`, `/dev/ttyACM0`).
    pub name: String,
    /// USB vendor ID, when known.
    pub vid: Option<u16>,
    /// USB product ID, when known.
    pub pid: Option<u16>,
    /// Manufacturer / product strings, when known.
    pub description: Option<String>,
}

/// Writes all of `data`, retrying partial and timed-out writes until
/// `limit` has passed. The port's own timeout is the short read timeout, so a
/// plain `write_all` would fail whenever the device or flow control holds
/// the line for a few milliseconds. Missing the deadline is `TimedOut`.
pub(crate) fn write_with_deadline(
    w: &mut impl Write,
    data: &[u8],
    limit: Duration,
) -> io::Result<()> {
    let deadline = std::time::Instant::now() + limit;
    let mut off = 0;
    while off < data.len() {
        match w.write(&data[off..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "port wrote 0 bytes",
                ))
            }
            Ok(n) => off += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("wrote {off} of {} bytes within {limit:?}", data.len()),
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Lists serial ports, with USB IDs where the platform provides them.
pub fn list_ports() -> Vec<PortInfo> {
    let mut out: Vec<PortInfo> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let mut info = PortInfo {
                name: p.port_name.clone(),
                vid: None,
                pid: None,
                description: None,
            };
            if let serialport::SerialPortType::UsbPort(u) = &p.port_type {
                info.vid = Some(u.vid);
                info.pid = Some(u.pid);
                info.description = match (&u.manufacturer, &u.product) {
                    (Some(m), Some(p)) => Some(format!("{m} {p}")),
                    (None, Some(p)) => Some(p.clone()),
                    (Some(m), None) => Some(m.clone()),
                    (None, None) => None,
                };
            }
            if info.vid.is_none() {
                if let Some((v, p, d)) = sysfs_usb_info(&info.name) {
                    info.vid = Some(v);
                    info.pid = Some(p);
                    info.description = d;
                }
            }
            info
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Reads USB IDs from sysfs (Linux without libudev).
#[cfg(target_os = "linux")]
fn sysfs_usb_info(port: &str) -> Option<(u16, u16, Option<String>)> {
    let name = port.rsplit('/').next()?;
    let mut dir = std::fs::canonicalize(format!("/sys/class/tty/{name}/device")).ok()?;
    for _ in 0..6 {
        let vid = std::fs::read_to_string(dir.join("idVendor"));
        let pid = std::fs::read_to_string(dir.join("idProduct"));
        if let (Ok(v), Ok(p)) = (vid, pid) {
            let v = u16::from_str_radix(v.trim(), 16).ok()?;
            let p = u16::from_str_radix(p.trim(), 16).ok()?;
            let prod = std::fs::read_to_string(dir.join("product"))
                .ok()
                .map(|s| s.trim().to_string());
            return Some((v, p, prod));
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn sysfs_usb_info(_port: &str) -> Option<(u16, u16, Option<String>)> {
    None
}

/// Picks the port for a profile: an explicit name, or with `"auto"` the
/// first port matching `usb_vid`/`usb_pid` (or the first port at all).
pub fn resolve_port(profile: &ConnectionProfile) -> Result<String, CoreError> {
    if profile.port != "auto" {
        return Ok(profile.port.clone());
    }
    let ports = list_ports();
    let found = ports.iter().find(|p| {
        profile.usb_vid.is_none_or(|v| p.vid == Some(v))
            && profile.usb_pid.is_none_or(|v| p.pid == Some(v))
    });
    match found {
        Some(p) => Ok(p.name.clone()),
        None => Err(CoreError::Transport(
            match (profile.usb_vid, profile.usb_pid) {
                (Some(v), Some(p)) => format!("no serial port with USB ID {v:04x}:{p:04x}"),
                (Some(v), None) => format!("no serial port with USB vendor {v:04x}"),
                _ => "no serial ports found".into(),
            },
        )),
    }
}

/// An open serial port.
pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    name: String,
    baud: u32,
}

impl SerialTransport {
    /// Opens the port described by the profile.
    pub fn open(profile: &ConnectionProfile) -> Result<SerialTransport, CoreError> {
        let name = resolve_port(profile)?;
        let data_bits = match profile.data_bits {
            5 => serialport::DataBits::Five,
            6 => serialport::DataBits::Six,
            7 => serialport::DataBits::Seven,
            _ => serialport::DataBits::Eight,
        };
        let parity = match profile.parity {
            Parity::None => serialport::Parity::None,
            Parity::Odd => serialport::Parity::Odd,
            Parity::Even => serialport::Parity::Even,
        };
        let stop_bits = if profile.stop_bits == 2 {
            serialport::StopBits::Two
        } else {
            serialport::StopBits::One
        };
        let flow = match profile.flow_control {
            FlowControl::None => serialport::FlowControl::None,
            FlowControl::Software => serialport::FlowControl::Software,
            FlowControl::Hardware => serialport::FlowControl::Hardware,
        };
        let mut port = serialport::new(&name, profile.baud)
            .data_bits(data_bits)
            .parity(parity)
            .stop_bits(stop_bits)
            .flow_control(flow)
            .timeout(READ_TIMEOUT)
            .open()
            .map_err(|e| CoreError::Transport(format!("{name}: {e}")))?;
        // Not every adapter supports modem lines; ignore failures here.
        let _ = port.write_data_terminal_ready(profile.dtr_on_open);
        let _ = port.write_request_to_send(profile.rts_on_open);
        Ok(SerialTransport {
            port,
            name,
            baud: profile.baud,
        })
    }
}

impl Transport for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(e),
        }
    }
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        write_with_deadline(&mut self.port, data, WRITE_TIMEOUT)?;
        self.port.flush()
    }
    fn set_dtr(&mut self, on: bool) -> io::Result<()> {
        self.port
            .write_data_terminal_ready(on)
            .map_err(|e| io::Error::other(e.to_string()))
    }
    fn set_rts(&mut self, on: bool) -> io::Result<()> {
        self.port
            .write_request_to_send(on)
            .map_err(|e| io::Error::other(e.to_string()))
    }
    fn describe(&self) -> String {
        format!("{} @ {}", self.name, self.baud)
    }
}

/// Helper for "pulse reset": the pulse duration as a `Duration`.
pub fn pulse(ms: u64) -> Duration {
    Duration::from_millis(ms.clamp(1, 5000))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts a few bytes per call and times out every other call.
    struct Slow {
        got: Vec<u8>,
        calls: u32,
        stall: bool,
    }

    impl Write for Slow {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.stall || self.calls.is_multiple_of(2) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "busy"));
            }
            let n = b.len().min(3);
            self.got.extend_from_slice(&b[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_survive_partial_and_timed_out_chunks() {
        let mut w = Slow {
            got: Vec::new(),
            calls: 0,
            stall: false,
        };
        let data: Vec<u8> = (0..100).collect();
        write_with_deadline(&mut w, &data, Duration::from_secs(1)).unwrap();
        assert_eq!(w.got, data);
        let mut w = Slow {
            got: Vec::new(),
            calls: 0,
            stall: true,
        };
        let e = write_with_deadline(&mut w, &data, Duration::from_millis(20)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn explicit_port_is_used_verbatim() {
        let p = ConnectionProfile {
            port: "/dev/ttyFAKE9".into(),
            ..Default::default()
        };
        assert_eq!(resolve_port(&p).unwrap(), "/dev/ttyFAKE9");
        assert!(SerialTransport::open(&p).is_err());
    }

    #[test]
    fn auto_with_unknown_vid_fails_cleanly() {
        let p = ConnectionProfile {
            usb_vid: Some(0xFFFE),
            usb_pid: Some(0xFFFD),
            ..Default::default()
        };
        let e = resolve_port(&p).unwrap_err();
        assert!(e.to_string().contains("fffe:fffd"), "{e}");
    }

    #[test]
    fn listing_does_not_panic() {
        let _ = list_ports();
    }
}
