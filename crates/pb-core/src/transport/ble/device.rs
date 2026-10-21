//! The btleplug side of the BLE transport.

use std::io;
use std::pin::Pin;
use std::time::{Duration, Instant};

use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures_util::{Stream, StreamExt};
use tokio::runtime::Runtime;
use uuid::Uuid;

use super::{chunk_len, BleDevice, BleOptions};
use crate::error::CoreError;
use crate::transport::{Transport, READ_TIMEOUT};

type Notifications = Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

fn runtime() -> Result<Runtime, CoreError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CoreError::Transport(format!("BLE runtime: {e}")))
}

fn t(e: impl std::fmt::Display) -> CoreError {
    CoreError::Transport(format!("BLE: {e}"))
}

async fn adapter() -> Result<Adapter, CoreError> {
    let stack = |e: btleplug::Error| {
        t(format!(
            "cannot reach the Bluetooth stack ({e}){}",
            if cfg!(target_os = "linux") {
                "; is bluetoothd (BlueZ) running?"
            } else {
                ""
            }
        ))
    };
    let m = Manager::new().await.map_err(stack)?;
    m.adapters()
        .await
        .map_err(stack)?
        .into_iter()
        .next()
        .ok_or_else(|| t("no Bluetooth adapter"))
}

/// Scans and lists the devices that advertised.
pub fn scan(dur: Duration) -> Result<Vec<BleDevice>, CoreError> {
    let rt = runtime()?;
    rt.block_on(async {
        let a = adapter().await?;
        a.start_scan(ScanFilter::default()).await.map_err(t)?;
        tokio::time::sleep(dur).await;
        let _ = a.stop_scan().await;
        let mut out = Vec::new();
        for p in a.peripherals().await.map_err(t)? {
            let props = p.properties().await.map_err(t)?.unwrap_or_default();
            out.push(BleDevice {
                name: props.local_name.or(props.advertisement_name),
                address: p.address().to_string(),
                rssi: props.rssi,
                services: props.services.iter().map(Uuid::to_string).collect(),
            });
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.rssi));
        Ok(out)
    })
}

/// A connected UART-style GATT service.
pub struct BleTransport {
    rt: Runtime,
    peripheral: Peripheral,
    rx: Characteristic,
    tx: Uuid,
    notes: Notifications,
    chunk: usize,
    with_response: bool,
    pending: Vec<u8>,
    last_check: Instant,
    name: String,
}

impl BleTransport {
    /// Scans for the device, connects, finds the characteristics and
    /// subscribes to notifications.
    pub fn connect(o: &BleOptions) -> Result<BleTransport, CoreError> {
        let rt = runtime()?;
        let service = Uuid::parse_str(&o.service).map_err(t)?;
        let rx_id = Uuid::parse_str(&o.rx_char).map_err(t)?;
        let tx_id = Uuid::parse_str(&o.tx_char).map_err(t)?;
        let (peripheral, rx, notes, label) = rt.block_on(async {
            let a = adapter().await?;
            a.start_scan(ScanFilter {
                services: vec![service],
            })
            .await
            .map_err(t)?;
            let end = Instant::now() + o.scan;
            let found = loop {
                let mut hit = None;
                for p in a.peripherals().await.map_err(t)? {
                    let props = p.properties().await.map_err(t)?.unwrap_or_default();
                    let name = props
                        .local_name
                        .clone()
                        .or(props.advertisement_name.clone());
                    if o.matches(name.as_deref(), &p.address().to_string()) {
                        hit = Some((p, name));
                        break;
                    }
                }
                if let Some(h) = hit {
                    break h;
                }
                if Instant::now() >= end {
                    let _ = a.stop_scan().await;
                    return Err(t(format!(
                        "no device {} advertising {} within {:?}",
                        o.address.clone().or(o.name.clone()).unwrap_or_default(),
                        o.service,
                        o.scan
                    )));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            let _ = a.stop_scan().await;
            let (p, name) = found;
            p.connect().await.map_err(t)?;
            p.discover_services().await.map_err(t)?;
            let chars = p.characteristics();
            let rx = chars
                .iter()
                .find(|c| c.uuid == rx_id)
                .cloned()
                .ok_or_else(|| t(format!("no RX characteristic {rx_id}")))?;
            let tx = chars
                .iter()
                .find(|c| c.uuid == tx_id)
                .cloned()
                .ok_or_else(|| t(format!("no TX characteristic {tx_id}")))?;
            p.subscribe(&tx).await.map_err(t)?;
            let notes = p.notifications().await.map_err(t)?;
            let label = format!(
                "ble {} ({})",
                name.unwrap_or_else(|| "?".into()),
                p.address()
            );
            Ok::<_, CoreError>((p, rx, notes, label))
        })?;
        let chunk = chunk_len(peripheral.mtu());
        Ok(BleTransport {
            rt,
            peripheral,
            rx,
            tx: tx_id,
            notes,
            chunk,
            with_response: o.with_response,
            pending: Vec::new(),
            last_check: Instant::now(),
            name: label,
        })
    }
}

impl Transport for BleTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            let tx = self.tx;
            let notes = &mut self.notes;
            let got = self
                .rt
                .block_on(async { tokio::time::timeout(READ_TIMEOUT, notes.next()).await });
            match got {
                Ok(Some(n)) if n.uuid == tx => self.pending = n.value,
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "BLE notifications ended",
                    ))
                }
                Err(_) => {}
            }
            // A link that dropped quietly shows up here.
            if self.last_check.elapsed() > Duration::from_secs(1) {
                self.last_check = Instant::now();
                let p = self.peripheral.clone();
                if !self.rt.block_on(p.is_connected()).unwrap_or(false) {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "BLE device disconnected",
                    ));
                }
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let kind = if self.with_response {
            WriteType::WithResponse
        } else {
            WriteType::WithoutResponse
        };
        for part in data.chunks(self.chunk) {
            let p = self.peripheral.clone();
            let rx = self.rx.clone();
            self.rt
                .block_on(async move {
                    tokio::time::timeout(crate::transport::WRITE_TIMEOUT, p.write(&rx, part, kind))
                        .await
                })
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "BLE write timed out"))?
                .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()))?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} (MTU chunks of {})", self.name, self.chunk)
    }
}

impl Drop for BleTransport {
    fn drop(&mut self) {
        let p = self.peripheral.clone();
        let _ = self.rt.block_on(async move { p.disconnect().await });
    }
}
