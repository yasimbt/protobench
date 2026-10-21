//! Linux SocketCAN backend (`can0`, `vcan0`, any adapter with a kernel
//! driver: PEAK, Kvaser, gs_usb/candleLight, …).

use std::io;
use std::time::Duration;

use socketcan::id::FdFlags;
use socketcan::{
    CanAnyFrame, CanDataFrame, CanFdFrame, CanFdSocket, CanFrame as ScFrame, CanSocket,
    EmbeddedFrame, ExtendedId, Id, Socket, StandardId,
};

use super::frame::{CanBus, CanFrame};
use crate::error::CoreError;
use crate::transport::WRITE_TIMEOUT;

enum Sock {
    Classic(CanSocket),
    Fd(CanFdSocket),
}

/// A SocketCAN interface.
pub struct SocketCanBus {
    sock: Sock,
    name: String,
}

impl SocketCanBus {
    /// Opens `interface`, with CAN FD frames when `fd`.
    pub fn open(interface: &str, fd: bool) -> Result<SocketCanBus, CoreError> {
        let e = |e: io::Error| CoreError::Transport(format!("SocketCAN {interface}: {e}"));
        let sock = if fd {
            let s = CanFdSocket::open(interface).map_err(e)?;
            s.set_write_timeout(WRITE_TIMEOUT).map_err(e)?;
            Sock::Fd(s)
        } else {
            let s = CanSocket::open(interface).map_err(e)?;
            s.set_write_timeout(WRITE_TIMEOUT).map_err(e)?;
            Sock::Classic(s)
        };
        Ok(SocketCanBus {
            sock,
            name: format!("SocketCAN {interface}"),
        })
    }
}

fn id_of(id: Id) -> (u32, bool) {
    match id {
        Id::Standard(s) => (u32::from(s.as_raw()), false),
        Id::Extended(x) => (x.as_raw(), true),
    }
}

fn data_frame(d: &CanDataFrame) -> CanFrame {
    let (id, extended) = id_of(d.id());
    CanFrame {
        id,
        extended,
        fd: false,
        brs: false,
        data: d.data().to_vec(),
    }
}

fn to_id(f: &CanFrame) -> io::Result<Id> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad CAN id {:#x}", f.id),
        )
    };
    Ok(if f.extended {
        Id::Extended(ExtendedId::new(f.id).ok_or_else(bad)?)
    } else {
        Id::Standard(StandardId::new(u16::try_from(f.id).map_err(|_| bad())?).ok_or_else(bad)?)
    })
}

fn timeout_none(e: io::Error) -> io::Result<Option<CanFrame>> {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
            Ok(None)
        }
        _ => Err(e),
    }
}

impl CanBus for SocketCanBus {
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<CanFrame>> {
        match &self.sock {
            Sock::Classic(s) => match s.read_frame_timeout(timeout) {
                Ok(ScFrame::Data(d)) => Ok(Some(data_frame(&d))),
                Ok(_) => Ok(None),
                Err(e) => timeout_none(e),
            },
            Sock::Fd(s) => match s.read_frame_timeout(timeout) {
                Ok(CanAnyFrame::Normal(d)) => Ok(Some(data_frame(&d))),
                Ok(CanAnyFrame::Fd(f)) => {
                    let (id, extended) = id_of(f.id());
                    Ok(Some(CanFrame {
                        id,
                        extended,
                        fd: true,
                        brs: f.is_brs(),
                        data: f.data().to_vec(),
                    }))
                }
                Ok(_) => Ok(None),
                Err(e) => timeout_none(e),
            },
        }
    }

    fn send(&mut self, f: &CanFrame) -> io::Result<()> {
        let id = to_id(f)?;
        let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "CAN frame data too long");
        match &self.sock {
            Sock::Classic(s) => {
                let d = CanDataFrame::new(id, &f.data).ok_or_else(too_long)?;
                s.write_frame(&d)
            }
            Sock::Fd(s) if f.fd => {
                let flags = if f.brs {
                    FdFlags::BRS
                } else {
                    FdFlags::empty()
                };
                let d = CanFdFrame::with_flags(id, &f.data, flags).ok_or_else(too_long)?;
                s.write_frame(&d)
            }
            Sock::Fd(s) => {
                let d = CanDataFrame::new(id, &f.data).ok_or_else(too_long)?;
                s.write_frame(&d)
            }
        }
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}
