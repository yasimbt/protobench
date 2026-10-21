//! slcan (Lawicel ASCII) CAN adapters over a serial port: CANable,
//! USBtin, CANtact and similar.
//!
//! ```text
//! S6\r          bit rate 500 kbit/s (S0 10k … S8 1M)
//! Y2\r          CAN FD data bit rate 2 Mbit/s (adapter specific)
//! O\r  C\r      open / close the channel
//! t1238DEADBEEF00112233\r     standard frame: id 123, DLC 8, data
//! T0000123A4…\r               extended frame: 8 hex digits of id
//! d / D         CAN FD frame without bit-rate switch (DLC 0–F)
//! b / B         CAN FD frame with bit-rate switch
//! \r = OK, \x07 = error; received frames use the same syntax.
//! ```

use std::io;
use std::time::{Duration, Instant};

use super::frame::{dlc_to_len, len_to_dlc, CanBus, CanFrame};
use crate::transport::Transport;

/// The `S` code of a bit rate.
pub fn bitrate_code(bps: u32) -> Option<char> {
    Some(match bps {
        10_000 => '0',
        20_000 => '1',
        50_000 => '2',
        100_000 => '3',
        125_000 => '4',
        250_000 => '5',
        500_000 => '6',
        800_000 => '7',
        1_000_000 => '8',
        _ => return None,
    })
}

/// The `Y` code of a CAN FD data bit rate (as CANable 2.0 firmware uses).
pub fn data_bitrate_code(bps: u32) -> Option<char> {
    Some(match bps {
        1_000_000 => '1',
        2_000_000 => '2',
        4_000_000 => '4',
        5_000_000 => '5',
        8_000_000 => '8',
        _ => return None,
    })
}

/// The slcan command or line for a frame (without the `\r`).
pub fn format_frame(f: &CanFrame) -> String {
    let kind = match (f.fd, f.brs, f.extended) {
        (false, _, false) => 't',
        (false, _, true) => 'T',
        (true, false, false) => 'd',
        (true, false, true) => 'D',
        (true, true, false) => 'b',
        (true, true, true) => 'B',
    };
    let mut s = String::with_capacity(12 + f.data.len() * 2);
    s.push(kind);
    if f.extended {
        s.push_str(&format!("{:08X}", f.id & 0x1FFF_FFFF));
    } else {
        s.push_str(&format!("{:03X}", f.id & 0x7FF));
    }
    let dlc = len_to_dlc(f.data.len()).unwrap_or(0);
    s.push(
        char::from_digit(u32::from(dlc), 16)
            .unwrap_or('0')
            .to_ascii_uppercase(),
    );
    for b in &f.data {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// Parses a frame line (without the `\r`). Ok(None) for lines that are
/// not frames (acknowledgements, version answers); a trailing 4-digit
/// timestamp is ignored.
pub fn parse_frame(line: &str) -> Result<Option<CanFrame>, String> {
    let Some(kind) = line.chars().next() else {
        return Ok(None);
    };
    let (fd, brs, extended) = match kind {
        't' => (false, false, false),
        'T' => (false, false, true),
        'd' => (true, false, false),
        'D' => (true, false, true),
        'b' => (true, true, false),
        'B' => (true, true, true),
        _ => return Ok(None),
    };
    let id_len = if extended { 8 } else { 3 };
    let hex = |s: &str| u32::from_str_radix(s, 16).map_err(|_| format!("bad slcan line `{line}`"));
    let id = hex(line
        .get(1..1 + id_len)
        .ok_or_else(|| format!("short slcan line `{line}`"))?)?;
    let dlc = hex(line
        .get(1 + id_len..2 + id_len)
        .ok_or_else(|| format!("short slcan line `{line}`"))?)? as u8;
    if !fd && dlc > 8 {
        return Err(format!("DLC {dlc} on a classic frame `{line}`"));
    }
    let n = if fd {
        dlc_to_len(dlc)
    } else {
        usize::from(dlc)
    };
    let start = 2 + id_len;
    let body = line
        .get(start..start + n * 2)
        .ok_or_else(|| format!("slcan line `{line}` shorter than its DLC"))?;
    let data = (0..n)
        .map(|i| u8::from_str_radix(&body[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| format!("bad data in slcan line `{line}`"))?;
    Ok(Some(CanFrame {
        id,
        extended,
        fd,
        brs,
        data,
    }))
}

/// An slcan adapter on a byte stream (normally a serial port).
pub struct SlcanBus {
    link: Box<dyn Transport>,
    buf: Vec<u8>,
    lines: std::collections::VecDeque<String>,
    name: String,
}

impl SlcanBus {
    /// Configures and opens the adapter: closes it (in case it was left
    /// open), sets the bit rate (and the FD data bit rate), opens it.
    pub fn open(
        link: Box<dyn Transport>,
        bitrate: u32,
        data_bitrate: Option<u32>,
    ) -> io::Result<SlcanBus> {
        let name = format!("slcan {} @ {} kbit/s", link.describe(), bitrate / 1000);
        let mut bus = SlcanBus {
            link,
            buf: Vec::new(),
            lines: Default::default(),
            name,
        };
        // Answers to "C" vary (BEL when already closed): ignore it.
        bus.link.write_all(b"C\r")?;
        let _ = bus.answer(Duration::from_millis(100));
        let code = bitrate_code(bitrate).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("slcan has no code for {bitrate} bit/s"),
            )
        })?;
        bus.command(&format!("S{code}"))?;
        if let Some(d) = data_bitrate {
            let c = data_bitrate_code(d).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("slcan has no code for a {d} bit/s data rate"),
                )
            })?;
            bus.command(&format!("Y{c}"))?;
        }
        bus.command("O")?;
        Ok(bus)
    }

    /// Sends a command and waits for its `\r` (or BEL = refused).
    fn command(&mut self, cmd: &str) -> io::Result<()> {
        self.link.write_all(format!("{cmd}\r").as_bytes())?;
        match self.answer(Duration::from_millis(500))? {
            Some(true) => Ok(()),
            Some(false) => Err(io::Error::other(format!(
                "the slcan adapter refused `{cmd}`"
            ))),
            None => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer to `{cmd}`: is this an slcan adapter?"),
            )),
        }
    }

    /// Reads until an OK (`\r` alone) or error (BEL) answer; frame lines
    /// that arrive meanwhile are kept.
    fn answer(&mut self, limit: Duration) -> io::Result<Option<bool>> {
        let end = Instant::now() + limit;
        let mut tmp = [0u8; 512];
        loop {
            while let Some(i) = self.buf.iter().position(|&b| b == b'\r' || b == 0x07) {
                let ok = self.buf[i] == b'\r';
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                if !ok {
                    return Ok(Some(false));
                }
                if text.is_empty() || text == "z" || text == "Z" {
                    return Ok(Some(true));
                }
                self.lines.push_back(text);
            }
            if Instant::now() >= end {
                return Ok(None);
            }
            let n = self.link.read(&mut tmp)?;
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
}

impl CanBus for SlcanBus {
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<CanFrame>> {
        let end = Instant::now() + timeout;
        let mut tmp = [0u8; 4096];
        loop {
            while let Some(line) = self.lines.pop_front() {
                match parse_frame(&line) {
                    Ok(Some(f)) => return Ok(Some(f)),
                    Ok(None) => {}
                    Err(e) => tracing::debug!("{e}"),
                }
            }
            while let Some(i) = self.buf.iter().position(|&b| b == b'\r' || b == 0x07) {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                if line.last() == Some(&b'\r') && line.len() > 1 {
                    self.lines
                        .push_back(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
                }
            }
            if !self.lines.is_empty() {
                continue;
            }
            if self.buf.len() > 1024 {
                // No line end in a kilobyte: not slcan.
                self.buf.clear();
            }
            let n = self.link.read(&mut tmp)?;
            self.buf.extend_from_slice(&tmp[..n]);
            if n == 0 && Instant::now() >= end {
                return Ok(None);
            }
        }
    }

    fn send(&mut self, f: &CanFrame) -> io::Result<()> {
        let mut s = format_frame(f);
        s.push('\r');
        self.link.write_all(s.as_bytes())
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

impl Drop for SlcanBus {
    fn drop(&mut self) {
        let _ = self.link.write_all(b"C\r");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(id: u32, ext: bool, fd: bool, brs: bool, data: &[u8]) -> CanFrame {
        CanFrame {
            id,
            extended: ext,
            fd,
            brs,
            data: data.to_vec(),
        }
    }

    #[test]
    fn lines_round_trip() {
        let cases = [
            (f(0x123, false, false, false, &[0xDE, 0xAD]), "t1232DEAD"),
            (f(0x7E0, false, false, false, &[]), "t7E00"),
            (
                f(0x18DAF110, true, false, false, &[1, 2, 3]),
                "T18DAF1103010203",
            ),
            (
                f(0x10, false, true, false, &[0xAA; 12]),
                "d0109AAAAAAAAAAAAAAAAAAAAAAAA",
            ),
            (
                f(0x10, true, true, true, &[0; 64]),
                &format!("B00000010F{}", "00".repeat(64)),
            ),
        ];
        for (frame, line) in cases {
            assert_eq!(format_frame(&frame), line);
            assert_eq!(parse_frame(line).unwrap(), Some(frame));
        }
        // A trailing timestamp is ignored.
        assert_eq!(
            parse_frame("t1232DEADBEEF").unwrap(),
            Some(f(0x123, false, false, false, &[0xDE, 0xAD]))
        );
        assert_eq!(parse_frame("z").unwrap(), None);
        assert_eq!(parse_frame("V1013").unwrap(), None);
        assert!(parse_frame("t12").is_err());
        assert!(parse_frame("t1239").is_err(), "classic DLC above 8");
        assert!(parse_frame("t1234AABB").is_err(), "shorter than its DLC");
        assert!(parse_frame("t12G0").is_err());
    }

    #[test]
    fn codes() {
        assert_eq!(bitrate_code(500_000), Some('6'));
        assert_eq!(bitrate_code(1_000_000), Some('8'));
        assert_eq!(bitrate_code(333_000), None);
        assert_eq!(data_bitrate_code(2_000_000), Some('2'));
    }
}
