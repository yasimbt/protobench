//! CAN transport: protobuf messages over CAN with ISO-TP segmentation (or
//! one message per frame), through an slcan adapter (any OS) or a Linux
//! SocketCAN interface.
//!
//! The host sends with `tx_id` and listens on `rx_id`; frames with other
//! identifiers are ignored. Every reassembled ISO-TP message (or every
//! frame, with `isotp = false`) is one frame for the session, so the
//! framing is normally `none`.

mod frame;
pub mod isotp;
pub mod slcan;
#[cfg(target_os = "linux")]
mod socket;

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

pub use frame::{dlc_to_len, fd_len, len_to_dlc, CanBus, CanFrame, FD_LENS};
use isotp::{
    consecutive_frame, first_frame, single_frame, st_min_duration, FlowControl, IsoTpParams,
    Receiver, RxEvent,
};
pub use slcan::SlcanBus;
#[cfg(target_os = "linux")]
pub use socket::SocketCanBus;

use super::{Transport, READ_TIMEOUT};
use crate::config::{CanBackend, ConnectionProfile};
use crate::error::CoreError;

/// Most flow-control WAIT frames accepted in a row (N_WFTmax).
pub const MAX_WAITS: u32 = 16;

/// Largest ISO-TP message accepted (the framing's `max_frame_len` limit
/// applies on top).
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Addressing and segmentation of a CAN link.
#[derive(Debug, Clone, PartialEq)]
pub struct CanOptions {
    /// Identifier this end sends with.
    pub tx_id: u32,
    /// Identifier this end listens on.
    pub rx_id: u32,
    /// 29-bit identifiers.
    pub extended: bool,
    /// CAN FD frames.
    pub fd: bool,
    /// Bit-rate switch on FD frames.
    pub brs: bool,
    /// ISO-TP settings, or None for one message per frame.
    pub isotp: Option<IsoTpParams>,
}

impl CanOptions {
    /// Options of a profile (the host's side).
    pub fn from_profile(p: &ConnectionProfile) -> Result<CanOptions, CoreError> {
        let (Some(tx_id), Some(rx_id)) = (p.tx_id, p.rx_id) else {
            return Err(CoreError::Transport(
                "a can profile needs `tx_id` and `rx_id`".into(),
            ));
        };
        Ok(CanOptions {
            tx_id,
            rx_id,
            extended: p.extended_id,
            fd: p.fd,
            brs: p.fd && p.data_bitrate.is_some(),
            isotp: p.isotp.then(|| IsoTpParams {
                frame_len: if p.fd { 64 } else { 8 },
                padding: p.isotp_padding,
                block_size: p.isotp_block_size,
                st_min: isotp::st_min_from_us(p.isotp_st_min_us),
                timeout: Duration::from_millis(p.isotp_timeout_ms.max(1)),
                max_len: MAX_MESSAGE,
            }),
        })
    }

    /// The other end's options: identifiers swapped.
    pub fn peer(&self) -> CanOptions {
        CanOptions {
            tx_id: self.rx_id,
            rx_id: self.tx_id,
            ..self.clone()
        }
    }
}

/// A CAN link carrying whole messages.
pub struct CanTransport {
    bus: Box<dyn CanBus>,
    o: CanOptions,
    rx: Option<Receiver>,
    inbox: VecDeque<Vec<u8>>,
    /// Frames for others or flow control seen while not sending.
    ignored: u64,
}

impl CanTransport {
    /// Wraps an opened bus.
    pub fn new(bus: Box<dyn CanBus>, o: CanOptions) -> CanTransport {
        CanTransport {
            rx: o.isotp.clone().map(Receiver::new),
            bus,
            o,
            inbox: VecDeque::new(),
            ignored: 0,
        }
    }

    fn frame(&self, data: Vec<u8>) -> CanFrame {
        CanFrame {
            id: self.o.tx_id,
            extended: self.o.extended,
            fd: self.o.fd,
            brs: self.o.brs,
            data,
        }
    }

    fn mine(&self, f: &CanFrame) -> bool {
        f.id == self.o.rx_id && f.extended == self.o.extended
    }

    /// Handles a received frame; flow control for our own sending is
    /// returned instead.
    fn on_frame(&mut self, f: CanFrame) -> io::Result<Option<FlowControl>> {
        if !self.mine(&f) {
            self.ignored += 1;
            return Ok(None);
        }
        let Some(rx) = self.rx.as_mut() else {
            self.inbox.push_back(f.data);
            return Ok(None);
        };
        if let Some(fc) = FlowControl::parse(&f.data) {
            return Ok(Some(fc));
        }
        for ev in rx.on_frame(&f.data, Instant::now()) {
            match ev {
                RxEvent::Message(m) => self.inbox.push_back(m),
                RxEvent::Send(fc) => {
                    let fr = self.frame(fc);
                    self.bus.send(&fr)?;
                }
                RxEvent::Error(e) => tracing::warn!("ISO-TP: {e}"),
            }
        }
        Ok(None)
    }

    /// Waits for the receiver's flow control, handling other frames.
    fn wait_flow_control(&mut self, limit: Duration) -> io::Result<FlowControl> {
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("ISO-TP: no flow control within {limit:?}"),
                ));
            }
            if let Some(f) = self.bus.recv(left.min(READ_TIMEOUT))? {
                if let Some(fc) = self.on_frame(f)? {
                    return Ok(fc);
                }
            }
        }
    }

    fn send_isotp(&mut self, data: &[u8], p: &IsoTpParams) -> io::Result<()> {
        if let Some(sf) = single_frame(data, p) {
            let f = self.frame(sf);
            return self.bus.send(&f);
        }
        if data.len() > p.max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("message of {} bytes exceeds the ISO-TP limit", data.len()),
            ));
        }
        let (ff, mut off) = first_frame(data, p);
        let f = self.frame(ff);
        self.bus.send(&f)?;
        let mut sn = 1u8;
        let mut waits = 0;
        while off < data.len() {
            let (bs, st) = match self.wait_flow_control(p.timeout)? {
                FlowControl::Continue { block_size, st_min } => {
                    (block_size, st_min_duration(st_min))
                }
                FlowControl::Wait => {
                    waits += 1;
                    if waits > MAX_WAITS {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "ISO-TP: the receiver kept asking to wait",
                        ));
                    }
                    continue;
                }
                FlowControl::Overflow => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("ISO-TP: the device cannot take {} bytes", data.len()),
                    ))
                }
            };
            waits = 0;
            let mut in_block = 0u8;
            while off < data.len() {
                let (cf, n) = consecutive_frame(sn, &data[off..], p);
                off += n;
                sn = (sn + 1) & 0x0F;
                let f = self.frame(cf);
                self.bus.send(&f)?;
                in_block = in_block.wrapping_add(1);
                if off >= data.len() || (bs != 0 && in_block == bs) {
                    break;
                }
                if !st.is_zero() {
                    std::thread::sleep(st);
                }
            }
        }
        Ok(())
    }
}

impl Transport for CanTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inbox.is_empty() {
            let mut wait = READ_TIMEOUT;
            // Take what is there, then return.
            for _ in 0..256 {
                match self.bus.recv(wait)? {
                    Some(f) => {
                        // Flow control that arrives late belongs to nothing.
                        let _ = self.on_frame(f)?;
                    }
                    None => break,
                }
                wait = Duration::ZERO;
                if !self.inbox.is_empty() {
                    break;
                }
            }
            if let Some(RxEvent::Error(e)) = self.rx.as_mut().and_then(|r| r.poll(Instant::now())) {
                tracing::warn!("ISO-TP: {e}");
            }
        }
        let Some(m) = self.inbox.pop_front() else {
            return Ok(0);
        };
        let n = m.len().min(buf.len());
        buf[..n].copy_from_slice(&m[..n]);
        Ok(n)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self.o.isotp.clone() {
            Some(p) => self.send_isotp(data, &p),
            None => {
                let max = if self.o.fd { 64 } else { 8 };
                if data.len() > max {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "{} bytes do not fit one CAN frame (max {max}); turn isotp on",
                            data.len()
                        ),
                    ));
                }
                let mut d = data.to_vec();
                if self.o.fd && d.len() > 8 {
                    d.resize(fd_len(d.len()), 0);
                }
                let f = self.frame(d);
                self.bus.send(&f)
            }
        }
    }

    fn describe(&self) -> String {
        format!(
            "{} tx {:#x} rx {:#x}{}",
            self.bus.describe(),
            self.o.tx_id,
            self.o.rx_id,
            if self.o.isotp.is_some() {
                " (ISO-TP)"
            } else {
                ""
            }
        )
    }

    fn message_based(&self) -> bool {
        true
    }
}

/// Opens the CAN interface of a profile: an slcan adapter on its serial
/// port, or a SocketCAN interface.
pub fn open_bus(p: &ConnectionProfile) -> Result<Box<dyn CanBus>, CoreError> {
    match p.can_backend {
        CanBackend::Slcan => {
            let link = super::SerialTransport::open(p)?;
            let bus = SlcanBus::open(Box::new(link), p.bitrate, p.data_bitrate.filter(|_| p.fd))
                .map_err(|e| CoreError::Transport(format!("{}: {e}", p.port)))?;
            Ok(Box::new(bus))
        }
        #[cfg(target_os = "linux")]
        CanBackend::Socketcan => Ok(Box::new(SocketCanBus::open(&p.interface, p.fd)?)),
        #[cfg(not(target_os = "linux"))]
        CanBackend::Socketcan => Err(CoreError::Transport(
            "SocketCAN is Linux only; use can_backend = \"slcan\"".into(),
        )),
    }
}

/// A connector for a CAN profile.
pub fn connector(
    p: &ConnectionProfile,
    _project_dir: &Path,
) -> Result<super::Connector, CoreError> {
    let o = CanOptions::from_profile(p)?;
    let p = p.clone();
    Ok(Box::new(move || {
        let bus = open_bus(&p)?;
        Ok(Box::new(CanTransport::new(bus, o.clone())) as Box<dyn Transport>)
    }))
}

#[cfg(test)]
mod tests;
