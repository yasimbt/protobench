//! Several devices at once (`[[devices]]`): one session per device, all on
//! one clock and one telemetry store, so every channel shares a time axis
//! and other devices' channels appear as `<device>.<channel>`.
//!
//! A [`Bench`] holds the running sessions of a project's devices, records
//! them into one file and lists them for sequence runs. The CLI, the GUI
//! and the Python bindings use it; the simulator transport comes from the
//! caller (`pb-sim` depends on this crate, not the other way round).

use std::path::Path;
use std::sync::Arc;

use crate::clock::{SharedClock, SystemClock};
use crate::config::{ConnectionProfile, TransportKind, MAIN_DEVICE};
use crate::error::CoreError;
use crate::project::Project;
use crate::recorder::{SessionWriter, SharedRecording};
use crate::sequence::Target;
use crate::session::{Session, SessionParams};
use crate::telemetry::TelemetryStore;
use crate::transport::{connector_for, SimFactory};

/// Builds the simulator transport for a device project (`pb_sim::sim_factory`).
pub type SimMaker<'a> = &'a dyn Fn(&Project) -> Result<SimFactory, CoreError>;

/// A running device.
pub struct BenchDevice {
    /// Device name.
    pub name: String,
    /// The device's project.
    pub project: Project,
    /// The profile it connected with.
    pub profile: ConnectionProfile,
    /// Its session.
    pub session: Arc<Session>,
}

/// The running devices of one project.
pub struct Bench {
    clock: SharedClock,
    telemetry: Arc<TelemetryStore>,
    devices: Vec<BenchDevice>,
    recording: Option<SharedRecording>,
}

impl Default for Bench {
    fn default() -> Self {
        Self::new(crate::telemetry::DEFAULT_CAPACITY)
    }
}

impl Bench {
    /// An empty bench whose channels hold `capacity` samples each.
    pub fn new(capacity: usize) -> Bench {
        Bench {
            clock: SystemClock::shared(),
            telemetry: Arc::new(TelemetryStore::new(capacity)),
            devices: Vec::new(),
            recording: None,
        }
    }

    /// The clock every device shares.
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    /// The telemetry store every device writes into.
    pub fn telemetry(&self) -> &Arc<TelemetryStore> {
        &self.telemetry
    }

    /// Running devices, in start order.
    pub fn devices(&self) -> &[BenchDevice] {
        &self.devices
    }

    /// A running device by name.
    pub fn get(&self, name: &str) -> Option<&BenchDevice> {
        self.devices.iter().find(|d| d.name == name)
    }

    /// The session of a running device.
    pub fn session(&self, name: &str) -> Option<&Arc<Session>> {
        self.get(name).map(|d| &d.session)
    }

    /// True if nothing is running.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Starts device `device` of `project` (`main` or a `[[devices]]`
    /// name) with `profile` (else the `[[devices]]` profile, else the
    /// device's default) and an optional port override. `edit` may adjust
    /// the session parameters (e.g. turn off auto-reconnect). A device that
    /// is already running is stopped first. If a recording is running, the
    /// new session joins it.
    pub fn start(
        &mut self,
        project: &Project,
        device: &str,
        profile: Option<&str>,
        port: Option<String>,
        sim: SimMaker,
        edit: impl FnOnce(&mut SessionParams),
    ) -> Result<&BenchDevice, CoreError> {
        let d = project.device(device).ok_or_else(|| {
            CoreError::Config(format!(
                "no device `{device}` (this project has {})",
                project
                    .all_devices()
                    .iter()
                    .map(|d| format!("`{}`", d.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        let prof = project.device_profile(device, profile)?;
        let dp = d.project.clone();
        let factory = if prof.transport == TransportKind::Sim {
            Some(sim(&dp)?)
        } else {
            None
        };
        let connector = connector_for(device, &prof, &dp.dir, &dp.config.framing, factory, port)?;
        let mut params = SessionParams::new(&dp, &prof)?.for_device(device, self.telemetry.clone());
        params.capacity = self.telemetry.capacity();
        edit(&mut params);
        self.stop(device);
        let session = Session::start(params, connector, self.clock.clone())?;
        if let Some(sink) = self.recording.as_ref().and_then(|r| r.sink(device)) {
            session.start_recording(sink);
        }
        self.devices.push(BenchDevice {
            name: device.to_string(),
            project: dp,
            profile: prof,
            session: Arc::new(session),
        });
        Ok(self.devices.last().expect("just pushed"))
    }

    /// Stops a device (its part of a running recording is flushed).
    pub fn stop(&mut self, device: &str) {
        if let Some(i) = self.devices.iter().position(|d| d.name == device) {
            let d = self.devices.remove(i);
            if let Some(mut sink) = d.session.stop_recording() {
                let _ = sink.finish();
            }
        }
    }

    /// Stops every device and any recording.
    pub fn stop_all(&mut self) {
        let _ = self.stop_recording();
        self.devices.clear();
    }

    /// The devices of `project` for a sequence run: every device it
    /// configures, with the session of those that are running.
    pub fn targets<'a>(&'a self, project: &'a Project) -> Vec<Target<'a>> {
        project
            .all_devices()
            .into_iter()
            .map(|d| Target {
                name: d.name,
                session: self.session(d.name).map(|s| s.as_ref()),
                config: &d.project.config,
            })
            .collect()
    }

    /// Records every running device (and devices started later) into one
    /// file.
    pub fn start_recording(&mut self, path: &Path, project: &Project) -> Result<(), CoreError> {
        self.stop_recording()?;
        let rec = SharedRecording::new(SessionWriter::create(path, project)?);
        for d in &self.devices {
            if let Some(sink) = rec.sink(&d.name) {
                d.session.start_recording(sink);
            }
        }
        self.recording = Some(rec);
        Ok(())
    }

    /// The running recording.
    pub fn recording(&self) -> Option<&SharedRecording> {
        self.recording.as_ref()
    }

    /// Stops recording and closes the file.
    pub fn stop_recording(&mut self) -> Result<(), CoreError> {
        let Some(rec) = self.recording.take() else {
            return Ok(());
        };
        let mut first_err = None;
        for d in &self.devices {
            if let Some(mut sink) = d.session.stop_recording() {
                if let Err(e) = sink.finish() {
                    first_err.get_or_insert(e);
                }
            }
        }
        if let Err(e) = rec.finish() {
            first_err.get_or_insert(e);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Adds a marker, recorded with `main` (or the first running device).
    pub fn add_marker(&self, text: &str) {
        if let Some(d) = self.get(MAIN_DEVICE).or(self.devices.first()) {
            d.session.add_marker(text);
        }
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        self.stop_all();
    }
}
