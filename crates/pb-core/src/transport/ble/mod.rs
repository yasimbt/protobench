//! BLE GATT transport: a UART-style service (Nordic UART Service by
//! default). The host writes to the device's RX characteristic and gets
//! notifications from its TX characteristic; together they are a byte
//! stream, so a framer (COBS, SLIP, a length prefix) splits the frames.
//!
//! btleplug, which drives the platform Bluetooth stacks, is async-only; the
//! transport (cargo feature `ble`, off by default) runs a current-thread
//! tokio runtime of its own on the session's I/O thread, so nothing else
//! in ProtoBench sees an async runtime.

#[cfg(feature = "ble")]
mod device;

use crate::config::ConnectionProfile;
use crate::error::CoreError;

/// Nordic UART Service.
pub const NUS_SERVICE: &str = "6e400001-b5a3-f393-e0a9-e50e24dcca9e";
/// NUS RX: the host writes here.
pub const NUS_RX: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
/// NUS TX: the device notifies here.
pub const NUS_TX: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";

/// True if this build can use Bluetooth.
pub const SUPPORTED: bool = cfg!(feature = "ble");

/// A 128-bit UUID in canonical lower-case form, from the canonical form or
/// a 16-bit SIG short form (`180d`, `0x180D`).
pub fn parse_uuid(s: &str) -> Result<String, String> {
    let t = s.trim().to_ascii_lowercase();
    let short = t.trim_start_matches("0x");
    if short.len() == 4 && short.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(format!("0000{short}-0000-1000-8000-00805f9b34fb"));
    }
    let groups: Vec<&str> = t.split('-').collect();
    let ok = groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()));
    if ok {
        Ok(t)
    } else {
        Err(format!(
            "`{s}` is not a UUID (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx or a 16-bit 180d)"
        ))
    }
}

/// A Bluetooth address in `AA:BB:CC:DD:EE:FF` form (`-` separators and
/// lower case accepted).
pub fn parse_address(s: &str) -> Result<String, String> {
    let t = s.trim().to_ascii_uppercase().replace('-', ":");
    let parts: Vec<&str> = t.split(':').collect();
    if parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
    {
        Ok(t)
    } else {
        Err(format!(
            "`{s}` is not a Bluetooth address (AA:BB:CC:DD:EE:FF)"
        ))
    }
}

/// Bytes per write for an ATT MTU: the MTU minus the 3-byte ATT header,
/// at least the 20 bytes every link allows.
pub fn chunk_len(mtu: u16) -> usize {
    usize::from(mtu.saturating_sub(3)).clamp(20, 512)
}

/// BLE settings of a profile.
#[derive(Debug, Clone, PartialEq)]
pub struct BleOptions {
    /// Advertised name to look for (exact match).
    pub name: Option<String>,
    /// Address to look for (wins over the name).
    pub address: Option<String>,
    /// Service UUID.
    pub service: String,
    /// Characteristic the host writes to.
    pub rx_char: String,
    /// Characteristic that notifies the host.
    pub tx_char: String,
    /// How long to scan for the device.
    pub scan: std::time::Duration,
    /// Write with response (acknowledged) instead of without.
    pub with_response: bool,
}

impl BleOptions {
    /// Validated options of a profile.
    pub fn from_profile(p: &ConnectionProfile) -> Result<BleOptions, String> {
        let address = p.ble_address.as_deref().map(parse_address).transpose()?;
        let name = p.ble_name.clone().filter(|n| !n.trim().is_empty());
        if address.is_none() && name.is_none() {
            return Err("ble needs `ble_name` or `ble_address`".into());
        }
        Ok(BleOptions {
            name,
            address,
            service: parse_uuid(&p.service)?,
            rx_char: parse_uuid(&p.rx_char)?,
            tx_char: parse_uuid(&p.tx_char)?,
            scan: std::time::Duration::from_secs(p.ble_scan_s.max(1)),
            with_response: p.ble_write_response,
        })
    }

    /// True if an advertised (name, address) is the device wanted.
    pub fn matches(&self, name: Option<&str>, address: &str) -> bool {
        match &self.address {
            Some(a) => a.eq_ignore_ascii_case(address),
            None => self.name.as_deref() == name,
        }
    }
}

/// A device seen by a scan.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BleDevice {
    /// Advertised name.
    pub name: Option<String>,
    /// Address (on macOS, the platform's identifier).
    pub address: String,
    /// Signal strength.
    pub rssi: Option<i16>,
    /// Advertised services.
    pub services: Vec<String>,
}

/// Scans for `seconds` and lists what advertised.
pub fn scan(seconds: u64) -> Result<Vec<BleDevice>, CoreError> {
    #[cfg(feature = "ble")]
    {
        device::scan(std::time::Duration::from_secs(seconds.max(1)))
    }
    #[cfg(not(feature = "ble"))]
    {
        let _ = seconds;
        Err(CoreError::Transport(
            "this build has no Bluetooth support (cargo feature `ble`)".into(),
        ))
    }
}

/// A connector for a BLE profile.
pub fn connector(p: &ConnectionProfile) -> Result<super::Connector, CoreError> {
    let o = BleOptions::from_profile(p).map_err(CoreError::Transport)?;
    #[cfg(feature = "ble")]
    {
        Ok(Box::new(move || {
            Ok(Box::new(device::BleTransport::connect(&o)?) as Box<dyn super::Transport>)
        }))
    }
    #[cfg(not(feature = "ble"))]
    {
        let _ = o;
        Err(CoreError::Transport(
            "this build has no Bluetooth support (cargo feature `ble`)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_addresses_and_chunks() {
        assert_eq!(
            parse_uuid("6E400001-B5A3-F393-E0A9-E50E24DCCA9E").unwrap(),
            NUS_SERVICE
        );
        assert_eq!(
            parse_uuid("0x180D").unwrap(),
            "0000180d-0000-1000-8000-00805f9b34fb"
        );
        assert!(parse_uuid("6e400001-b5a3-f393-e0a9").is_err());
        assert!(parse_uuid("zz00").is_err());
        assert_eq!(
            parse_address("c0-ff-ee:12:34:56").unwrap(),
            "C0:FF:EE:12:34:56"
        );
        assert!(parse_address("C0:FF:EE").is_err());
        assert_eq!(chunk_len(23), 20);
        assert_eq!(chunk_len(247), 244);
        assert_eq!(chunk_len(0), 20);
        assert_eq!(chunk_len(u16::MAX), 512);
        let data = [0u8; 50];
        let sizes: Vec<usize> = data.chunks(chunk_len(23)).map(<[u8]>::len).collect();
        assert_eq!(sizes, [20, 20, 10]);
    }

    #[test]
    fn options_from_a_profile() {
        let mut p = ConnectionProfile {
            transport: crate::config::TransportKind::Ble,
            ..Default::default()
        };
        assert!(BleOptions::from_profile(&p)
            .unwrap_err()
            .contains("ble_name"));
        p.ble_name = Some("Sensor-7".into());
        let o = BleOptions::from_profile(&p).unwrap();
        assert_eq!(
            (o.service.as_str(), o.rx_char.as_str(), o.tx_char.as_str()),
            (NUS_SERVICE, NUS_RX, NUS_TX)
        );
        assert!(o.matches(Some("Sensor-7"), "AA:BB:CC:DD:EE:FF"));
        assert!(!o.matches(Some("Sensor-8"), "AA:BB:CC:DD:EE:FF"));
        p.ble_address = Some("aa:bb:cc:dd:ee:ff".into());
        let o = BleOptions::from_profile(&p).unwrap();
        assert!(
            o.matches(None, "AA:BB:CC:DD:EE:FF"),
            "the address wins over the name"
        );
        p.service = "not-a-uuid".into();
        assert!(BleOptions::from_profile(&p).is_err());
    }
}
