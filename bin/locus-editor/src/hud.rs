//! Title-bar performance HUD: frame-rate / frame-time / RSS sampling and
//! the small number-formatters the menu bar uses.

use std::time::Instant;

/// Rolling performance stats for the title-bar HUD: frame rate plus process
/// resident-set size. `tick()` is called once per rendered frame.
///
/// Frame time is exponentially smoothed (90/10 weighting) so the displayed
/// FPS doesn't jitter. RSS is sampled on a cheap cadence (every ~500ms) so
/// we don't pay for a `/proc` parse every frame on Linux.
pub(crate) struct PerfStats {
    last_frame: Option<Instant>,
    /// Exponentially smoothed frame time, in seconds. 0.0 means no samples yet.
    smoothed_dt: f32,
    /// Most recent raw frame interval, in seconds.
    last_dt: f32,
    /// Last RSS reading, in bytes. `None` if the platform sample failed.
    rss_bytes: Option<usize>,
    /// When we last sampled RSS.
    last_rss_sample: Option<Instant>,
}

impl PerfStats {
    pub(crate) fn new() -> Self {
        Self {
            last_frame: None,
            smoothed_dt: 0.0,
            last_dt: 0.0,
            rss_bytes: None,
            last_rss_sample: None,
        }
    }

    pub(crate) fn tick(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt = (now - prev).as_secs_f32();
            self.last_dt = dt;
            // Clamp crazy gaps (e.g. app was idle for seconds) so the EMA
            // doesn't get dragged to 0 fps and take forever to recover.
            let dt_clamped = dt.min(0.5);
            self.smoothed_dt = if self.smoothed_dt == 0.0 {
                dt_clamped
            } else {
                self.smoothed_dt * 0.9 + dt_clamped * 0.1
            };
        }
        self.last_frame = Some(now);

        // Sample RSS at most ~2 Hz to avoid the syscall/proc parse per frame.
        let should_sample = match self.last_rss_sample {
            None => true,
            Some(t) => now.duration_since(t).as_millis() >= 500,
        };
        if should_sample {
            self.rss_bytes = memory_stats::memory_stats().map(|s| s.physical_mem);
            self.last_rss_sample = Some(now);
        }
    }

    pub(crate) fn fps(&self) -> f32 {
        if self.smoothed_dt > 0.0 {
            1.0 / self.smoothed_dt
        } else {
            0.0
        }
    }

    pub(crate) fn frame_ms(&self) -> f32 {
        self.last_dt * 1000.0
    }

    /// RSS formatted as e.g. `142.3 MB`. Empty string if sampling failed.
    pub(crate) fn rss_display(&self) -> String {
        match self.rss_bytes {
            Some(b) => format_bytes(b),
            None => String::from("— MB"),
        }
    }
}

/// Format an integer count with k/M suffixes for HUD density (e.g.
/// `1500` → `1.5k`, `2_300_000` → `2.3M`). Values below 1000 render as-is.
pub(crate) fn format_count(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format a byte count as a short human-readable string (KiB/MiB/GiB).
pub(crate) fn format_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}
