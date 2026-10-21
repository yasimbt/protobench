//! CAN frames and the bus interface the backends implement.

use std::io;
use std::time::Duration;

/// A CAN or CAN FD data frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFrame {
    /// Identifier (11 or 29 bits).
    pub id: u32,
    /// 29-bit identifier.
    pub extended: bool,
    /// CAN FD frame.
    pub fd: bool,
    /// CAN FD bit-rate switch.
    pub brs: bool,
    /// Data, up to 8 bytes (classic) or 64 (FD).
    pub data: Vec<u8>,
}

/// Data lengths CAN FD allows, by DLC.
pub const FD_LENS: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

/// The smallest valid CAN FD data length that holds `n` bytes.
pub fn fd_len(n: usize) -> usize {
    FD_LENS.iter().copied().find(|&l| l >= n).unwrap_or(64)
}

/// DLC of a data length (exact CAN FD lengths only).
pub fn len_to_dlc(n: usize) -> Option<u8> {
    FD_LENS.iter().position(|&l| l == n).map(|i| i as u8)
}

/// Data length of a DLC.
pub fn dlc_to_len(dlc: u8) -> usize {
    FD_LENS[usize::from(dlc & 0x0F)]
}

/// A CAN interface: an adapter or a socket.
pub trait CanBus: Send {
    /// Waits up to `timeout` for a frame. `Ok(None)` when none arrived; an
    /// error means the interface is gone.
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<CanFrame>>;
    /// Sends a frame.
    fn send(&mut self, f: &CanFrame) -> io::Result<()>;
    /// Short description.
    fn describe(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_lengths() {
        assert_eq!(fd_len(0), 0);
        assert_eq!(fd_len(8), 8);
        assert_eq!(fd_len(9), 12);
        assert_eq!(fd_len(33), 48);
        assert_eq!(fd_len(64), 64);
        assert_eq!(len_to_dlc(48), Some(14));
        assert_eq!(len_to_dlc(10), None);
        assert_eq!(dlc_to_len(15), 64);
    }
}
