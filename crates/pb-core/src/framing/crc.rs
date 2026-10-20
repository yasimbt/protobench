//! CRC variants appended to frame payloads.

use crate::config::{CrcKind, Endian};

const CRC8: crc::Crc<u8> = crc::Crc::<u8>::new(&crc::CRC_8_SMBUS);
const CRC16_CCITT_FALSE: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_IBM_3740);
const CRC16_MODBUS: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_MODBUS);
const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

/// A configured CRC: algorithm plus byte order of the appended value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc {
    /// Algorithm.
    pub kind: CrcKind,
    /// Byte order on the wire.
    pub endian: Endian,
}

impl Crc {
    /// Creates a CRC configuration.
    pub fn new(kind: CrcKind, endian: Endian) -> Self {
        Self { kind, endian }
    }

    /// Number of CRC bytes on the wire.
    pub fn len(&self) -> usize {
        match self.kind {
            CrcKind::None => 0,
            CrcKind::Crc8 => 1,
            CrcKind::Crc16CcittFalse | CrcKind::Crc16Modbus => 2,
            CrcKind::Crc32 => 4,
        }
    }

    /// True when no CRC is used.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Computes the checksum of `data`.
    pub fn compute(&self, data: &[u8]) -> u32 {
        match self.kind {
            CrcKind::None => 0,
            CrcKind::Crc8 => u32::from(CRC8.checksum(data)),
            CrcKind::Crc16CcittFalse => u32::from(CRC16_CCITT_FALSE.checksum(data)),
            CrcKind::Crc16Modbus => u32::from(CRC16_MODBUS.checksum(data)),
            CrcKind::Crc32 => CRC32.checksum(data),
        }
    }

    /// Appends the CRC of `out[start..]` to `out`.
    pub fn append(&self, out: &mut Vec<u8>, start: usize) {
        let n = self.len();
        if n == 0 {
            return;
        }
        let v = self.compute(&out[start..]);
        let le = v.to_le_bytes();
        match self.endian {
            Endian::Little => out.extend_from_slice(&le[..n]),
            Endian::Big => out.extend(le[..n].iter().rev()),
        }
    }

    /// Checks and strips the trailing CRC. Returns the payload, or the
    /// (expected, found) pair on mismatch. Frames shorter than the CRC fail.
    pub fn check<'a>(&self, frame: &'a [u8]) -> Result<&'a [u8], (u32, u32)> {
        let n = self.len();
        if n == 0 {
            return Ok(frame);
        }
        if frame.len() < n {
            return Err((0, 0));
        }
        let (payload, tail) = frame.split_at(frame.len() - n);
        let mut bytes = [0u8; 4];
        match self.endian {
            Endian::Little => bytes[..n].copy_from_slice(tail),
            Endian::Big => {
                for (i, b) in tail.iter().rev().enumerate() {
                    bytes[i] = *b;
                }
            }
        }
        let found = u32::from_le_bytes(bytes);
        let expected = self.compute(payload);
        if found == expected {
            Ok(payload)
        } else {
            Err((expected, found))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_check_values() {
        let data = b"123456789";
        let le = Endian::Little;
        assert_eq!(Crc::new(CrcKind::Crc8, le).compute(data), 0xF4);
        assert_eq!(Crc::new(CrcKind::Crc16CcittFalse, le).compute(data), 0x29B1);
        assert_eq!(Crc::new(CrcKind::Crc16Modbus, le).compute(data), 0x4B37);
        assert_eq!(Crc::new(CrcKind::Crc32, le).compute(data), 0xCBF4_3926);
    }

    #[test]
    fn append_and_check_both_endians() {
        for kind in [
            CrcKind::Crc8,
            CrcKind::Crc16CcittFalse,
            CrcKind::Crc16Modbus,
            CrcKind::Crc32,
        ] {
            for endian in [Endian::Little, Endian::Big] {
                let c = Crc::new(kind, endian);
                let mut v = b"123456789".to_vec();
                c.append(&mut v, 0);
                assert_eq!(v.len(), 9 + c.len());
                assert_eq!(c.check(&v).unwrap(), b"123456789");
                v[0] ^= 1;
                assert!(c.check(&v).is_err());
            }
        }
        let c = Crc::new(CrcKind::Crc16CcittFalse, Endian::Little);
        let mut v = b"123456789".to_vec();
        c.append(&mut v, 0);
        assert_eq!(&v[9..], &[0xB1, 0x29]);
        let c = Crc::new(CrcKind::Crc16CcittFalse, Endian::Big);
        let mut v = b"123456789".to_vec();
        c.append(&mut v, 0);
        assert_eq!(&v[9..], &[0x29, 0xB1]);
        assert!(c.check(&[0x01]).is_err());
    }
}
