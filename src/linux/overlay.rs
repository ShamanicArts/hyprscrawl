//! The live ink overlay (layer-shell, click-through). Placeholder until the wl_shm renderer lands.

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::capture::SharedCaptureTelemetry;
use crate::model::TargetInfo;

pub struct Overlay;

impl Overlay {
    pub fn start(_telemetry: Arc<SharedCaptureTelemetry>, _target: &TargetInfo) -> Result<Self> {
        bail!("overlay not built yet")
    }

    pub fn stop(self) {}
}
