//! Reference framing for the demo sensor in `no_std` Rust (documentation
//! only, not built with the workspace).
//!
//! Wire format, matching ProtoBench's `cobs` framer with
//! `crc = "crc16_ccitt_false"` and `crc_endian = "little"`:
//!
//! ```text
//! COBS( protobuf_bytes || crc_le ) || 0x00
//! ```
//!
//! Cargo.toml (firmware):
//!
//! ```toml
//! prost = { version = "0.14", default-features = false, features = ["derive"] }
//! cobs = { version = "0.5", default-features = false }
//! crc = "3"
//! ```
//!
//! Message types come from `prost-build` run on `device.proto` in the
//! firmware's build script (`demo::HostToDevice`, `demo::DeviceToHost`).

#![no_std]

use crc::{Crc, CRC_16_IBM_3740};
use prost::Message;

/// CRC-16/CCITT-FALSE (also catalogued as CRC-16/IBM-3740): poly 0x1021,
/// init 0xFFFF, no reflection, no final XOR. Check value 0x29B1.
const CRC16: Crc<u16> = Crc::<u16>::new(&CRC_16_IBM_3740);

/// project.toml: framing.max_frame_len
pub const MAX_FRAME_LEN: usize = 1024;

pub mod demo {
    include!(concat!(env!("OUT_DIR"), "/demo.rs"));
}

#[derive(Debug)]
pub enum FrameError {
    Encode,
    TooLong,
    Cobs,
    Crc,
    Decode,
}

/// Encodes a message into `out` as a complete frame, including the trailing
/// 0x00. Returns the frame length.
pub fn encode_frame(msg: &demo::DeviceToHost, out: &mut [u8]) -> Result<usize, FrameError> {
    let mut buf = [0u8; MAX_FRAME_LEN];
    let len = msg.encoded_len();
    if len + 2 > buf.len() {
        return Err(FrameError::TooLong);
    }
    let mut slice = &mut buf[..len];
    msg.encode(&mut slice).map_err(|_| FrameError::Encode)?;
    let crc = CRC16.checksum(&buf[..len]);
    buf[len..len + 2].copy_from_slice(&crc.to_le_bytes());
    let body = &buf[..len + 2];
    if out.len() < cobs::max_encoding_length(body.len()) + 1 {
        return Err(FrameError::TooLong);
    }
    let n = cobs::encode(body, out);
    out[n] = 0x00;
    Ok(n + 1)
}

/// Decodes the bytes received before a 0x00 delimiter.
pub fn decode_frame(frame: &[u8]) -> Result<demo::HostToDevice, FrameError> {
    let mut buf = [0u8; MAX_FRAME_LEN + 2];
    if frame.len() > buf.len() {
        return Err(FrameError::TooLong);
    }
    let n = cobs::decode(frame, &mut buf)
        .map_err(|_| FrameError::Cobs)?
        .frame_size();
    if n < 2 {
        return Err(FrameError::Crc);
    }
    let (payload, crc) = buf[..n].split_at(n - 2);
    if CRC16.checksum(payload).to_le_bytes() != [crc[0], crc[1]] {
        return Err(FrameError::Crc);
    }
    demo::HostToDevice::decode(payload).map_err(|_| FrameError::Decode)
}

/// Builds the reply to a request: the same `seq`, so ProtoBench can match it.
pub fn ack_for(req: &demo::HostToDevice, ok: bool) -> demo::DeviceToHost {
    demo::DeviceToHost {
        seq: req.seq,
        msg: Some(demo::device_to_host::Msg::Ack(demo::Ack { ok, error_code: 0 })),
    }
}
