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
    /// USB serial number, when the device reports one.
    pub serial_number: Option<String>,
    /// Manufacturer / product strings, when known.
    pub description: Option<String>,
}

impl PortInfo {
    /// `0483:5740`, or `----:----` for ports without USB IDs.
    pub fn usb_id(&self) -> String {
        match (self.vid, self.pid) {
            (Some(v), Some(p)) => format!("{v:04x}:{p:04x}"),
            _ => "----:----".into(),
        }
    }
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
                serial_number: None,
                description: None,
            };
            if let serialport::SerialPortType::UsbPort(u) = &p.port_type {
                info.vid = Some(u.vid);
                info.pid = Some(u.pid);
                info.serial_number = u
                    .serial_number
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                info.description = match (&u.manufacturer, &u.product) {
                    (Some(m), Some(p)) => Some(format!("{m} {p}")),
                    (None, Some(p)) => Some(p.clone()),
                    (Some(m), None) => Some(m.clone()),
                    (None, None) => None,
                };
            }
            if info.vid.is_none() {
                if let Some(u) = sysfs_usb_info(&info.name) {
                    info.vid = Some(u.vid);
                    info.pid = Some(u.pid);
                    info.serial_number = u.serial;
                    info.description = u.product;
                }
            }
            info
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// USB attributes read from sysfs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct SysfsUsb {
    vid: u16,
    pid: u16,
    serial: Option<String>,
    product: Option<String>,
}

/// Reads USB IDs from sysfs (Linux without libudev).
#[cfg(target_os = "linux")]
fn sysfs_usb_info(port: &str) -> Option<SysfsUsb> {
    let name = port.rsplit('/').next()?;
    let mut dir = std::fs::canonicalize(format!("/sys/class/tty/{name}/device")).ok()?;
    let read = |dir: &std::path::Path, f: &str| {
        std::fs::read_to_string(dir.join(f))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    for _ in 0..6 {
        let vid = std::fs::read_to_string(dir.join("idVendor"));
        let pid = std::fs::read_to_string(dir.join("idProduct"));
        if let (Ok(v), Ok(p)) = (vid, pid) {
            return Some(SysfsUsb {
                vid: u16::from_str_radix(v.trim(), 16).ok()?,
                pid: u16::from_str_radix(p.trim(), 16).ok()?,
                serial: read(&dir, "serial"),
                product: read(&dir, "product"),
            });
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn sysfs_usb_info(_port: &str) -> Option<SysfsUsb> {
    None
}

/// Picks the port for a profile from the attached ports (see
/// [`select_port`]).
pub fn resolve_port(profile: &ConnectionProfile) -> Result<String, CoreError> {
    if profile.port != "auto" {
        return Ok(profile.port.clone());
    }
    select_port(profile, &list_ports())
}

/// Picks the port for a profile from `ports`. An explicit name is used as
/// is. With `port = "auto"` exactly one port must match `usb_vid`,
/// `usb_pid` and `usb_serial` (serial numbers compare case-insensitively);
/// without any of them, the only USB serial port is taken (or the only
/// port, when none is USB). Several matches are an error that lists them,
/// so two identical boards never get mixed up.
pub fn select_port(profile: &ConnectionProfile, ports: &[PortInfo]) -> Result<String, CoreError> {
    if profile.port != "auto" {
        return Ok(profile.port.clone());
    }
    let serial = profile
        .usb_serial
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let filtered = profile.usb_vid.is_some() || profile.usb_pid.is_some() || serial.is_some();
    let matches: Vec<&PortInfo> = if filtered {
        ports
            .iter()
            .filter(|p| {
                profile.usb_vid.is_none_or(|v| p.vid == Some(v))
                    && profile.usb_pid.is_none_or(|v| p.pid == Some(v))
                    && serial.is_none_or(|s| {
                        p.serial_number
                            .as_deref()
                            .is_some_and(|n| n.trim().eq_ignore_ascii_case(s))
                    })
            })
            .collect()
    } else {
        let usb: Vec<&PortInfo> = ports.iter().filter(|p| p.vid.is_some()).collect();
        if usb.is_empty() {
            ports.iter().collect()
        } else {
            usb
        }
    };
    let what = match_description(profile.usb_vid, profile.usb_pid, serial);
    match matches.as_slice() {
        [one] => Ok(one.name.clone()),
        [] => Err(CoreError::Transport(match what {
            Some(w) => format!("no serial port with {w}"),
            None => "no serial ports found".into(),
        })),
        many => {
            let list = many
                .iter()
                .map(|p| match &p.serial_number {
                    Some(s) => format!("{} (serial {s})", p.name),
                    None => format!("{} (no serial number)", p.name),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let hint = if many.iter().all(|p| p.serial_number.is_none()) {
                "these ports report no USB serial numbers, so name the port (`port = \"COM5\"`) or pass --port"
            } else if serial.is_some() {
                "several ports report this serial number, so name the port or pass --port"
            } else {
                "set usb_serial in the profile to choose one (see `protobench list-ports`)"
            };
            Err(CoreError::Transport(format!(
                "{} serial ports match {}: {list}; {hint}",
                many.len(),
                what.unwrap_or_else(|| "port = \"auto\"".into())
            )))
        }
    }
}

/// `USB ID 0483:5740 and serial number X`, or None without any filter.
fn match_description(vid: Option<u16>, pid: Option<u16>, serial: Option<&str>) -> Option<String> {
    let id = match (vid, pid) {
        (Some(v), Some(p)) => Some(format!("USB ID {v:04x}:{p:04x}")),
        (Some(v), None) => Some(format!("USB vendor {v:04x}")),
        (None, Some(p)) => Some(format!("USB product {p:04x}")),
        (None, None) => None,
    };
    match (id, serial) {
        (Some(id), Some(s)) => Some(format!("{id} and serial number {s}")),
        (Some(id), None) => Some(id),
        (None, Some(s)) => Some(format!("USB serial number {s}")),
        (None, None) => None,
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

    fn port(name: &str, ids: Option<(u16, u16)>, serial: Option<&str>) -> PortInfo {
        PortInfo {
            name: name.into(),
            vid: ids.map(|i| i.0),
            pid: ids.map(|i| i.1),
            serial_number: serial.map(str::to_string),
            description: None,
        }
    }

    fn bench() -> Vec<PortInfo> {
        vec![
            port("/dev/ttyS0", None, None),
            port("/dev/ttyACM0", Some((0x0483, 0x5740)), Some("3971345A3130")),
            port("/dev/ttyACM1", Some((0x0483, 0x5740)), Some("3971345A3131")),
            port("/dev/ttyUSB0", Some((0x0403, 0x6001)), Some("FT1X2Y3Z")),
        ]
    }

    fn auto(vid: Option<u16>, pid: Option<u16>, serial: Option<&str>) -> ConnectionProfile {
        ConnectionProfile {
            usb_vid: vid,
            usb_pid: pid,
            usb_serial: serial.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn two_identical_boards_need_usb_serial() {
        let e = select_port(&auto(Some(0x0483), Some(0x5740), None), &bench()).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("2 serial ports match USB ID 0483:5740"), "{m}");
        assert!(
            m.contains("/dev/ttyACM0 (serial 3971345A3130)")
                && m.contains("/dev/ttyACM1 (serial 3971345A3131)"),
            "{m}"
        );
        assert!(m.contains("usb_serial"), "{m}");
        // The serial number picks one board; case does not matter.
        let p = auto(Some(0x0483), Some(0x5740), Some("3971345a3131"));
        assert_eq!(select_port(&p, &bench()).unwrap(), "/dev/ttyACM1");
        // A serial number alone is enough.
        let p = auto(None, None, Some("FT1X2Y3Z"));
        assert_eq!(select_port(&p, &bench()).unwrap(), "/dev/ttyUSB0");
        // A unique VID/PID still works without one.
        let p = auto(Some(0x0403), Some(0x6001), None);
        assert_eq!(select_port(&p, &bench()).unwrap(), "/dev/ttyUSB0");
        // An unknown serial number is reported with what was asked for.
        let e = select_port(&auto(Some(0x0483), None, Some("nope")), &bench()).unwrap_err();
        assert!(
            e.to_string()
                .contains("no serial port with USB vendor 0483 and serial number nope"),
            "{e}"
        );
        // An explicit port name never looks at the list.
        let named = ConnectionProfile {
            port: "COM7".into(),
            ..auto(Some(0x0483), None, None)
        };
        assert_eq!(select_port(&named, &bench()).unwrap(), "COM7");
    }

    #[test]
    fn auto_without_filters_takes_the_only_usb_port() {
        let ports = vec![
            port("/dev/ttyS0", None, None),
            port("/dev/ttyACM0", Some((0x2e8a, 0x000a)), None),
        ];
        assert_eq!(
            select_port(&auto(None, None, None), &ports).unwrap(),
            "/dev/ttyACM0"
        );
        // Only legacy ports: the only one is taken, several are an error.
        let one = vec![port("/dev/ttyS0", None, None)];
        assert_eq!(
            select_port(&auto(None, None, None), &one).unwrap(),
            "/dev/ttyS0"
        );
        let e = select_port(&auto(None, None, None), &bench()).unwrap_err();
        assert!(e.to_string().contains("3 serial ports match"), "{e}");
        assert_eq!(
            select_port(&auto(None, None, None), &[])
                .unwrap_err()
                .to_string(),
            "transport: no serial ports found"
        );
    }

    #[test]
    fn identical_boards_without_serial_numbers_must_be_named() {
        let ports = vec![
            port("COM3", Some((0x1a86, 0x7523)), None),
            port("COM4", Some((0x1a86, 0x7523)), None),
        ];
        let e = select_port(&auto(Some(0x1a86), Some(0x7523), None), &ports).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("COM3 (no serial number), COM4"), "{m}");
        assert!(m.contains("name the port"), "{m}");
    }
}
