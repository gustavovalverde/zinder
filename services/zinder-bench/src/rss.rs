//! Peak resident-set-size sampling with graceful platform degradation.

use serde::Serialize;

/// Source label used when a peak-RSS reading came from `/proc/self/status`.
pub const PEAK_RSS_SOURCE_PROC_VMHWM: &str = "proc_status_vmhwm";
/// Source label used when no peak-RSS reading is available on this platform.
pub const PEAK_RSS_SOURCE_UNAVAILABLE: &str = "unavailable";

/// A peak resident-set-size reading and where it came from.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PeakRss {
    /// Peak resident bytes, when the platform exposes a high-water mark.
    pub bytes: Option<u64>,
    /// Origin of the reading.
    pub source: &'static str,
}

/// Reads the process peak resident set size, degrading to `None` off Linux.
#[must_use]
pub fn peak_rss() -> PeakRss {
    #[cfg(target_os = "linux")]
    {
        linux_peak_rss()
    }
    #[cfg(not(target_os = "linux"))]
    {
        PeakRss {
            bytes: None,
            source: PEAK_RSS_SOURCE_UNAVAILABLE,
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_peak_rss() -> PeakRss {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return PeakRss {
            bytes: None,
            source: PEAK_RSS_SOURCE_UNAVAILABLE,
        };
    };
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("VmHWM:") else {
            continue;
        };
        let kib = rest
            .split_whitespace()
            .next()
            .and_then(|field| field.parse::<u64>().ok());
        if let Some(kib) = kib {
            return PeakRss {
                bytes: Some(kib.saturating_mul(1024)),
                source: PEAK_RSS_SOURCE_PROC_VMHWM,
            };
        }
    }
    PeakRss {
        bytes: None,
        source: PEAK_RSS_SOURCE_UNAVAILABLE,
    }
}
