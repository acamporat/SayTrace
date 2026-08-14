use std::time::Instant;

#[cfg(target_os = "macos")]
use apple_log::{OSSignpostInterval, OSSignposter, CATEGORY_POINTS_OF_INTEREST};

/// A lightweight, privacy-safe timing span.
///
/// On macOS this emits an Instruments-compatible interval signpost. All
/// platforms also emit a bounded structured log line so physical runs can be
/// compared without recording titles, transcript text, or filesystem paths.
pub struct PerformanceSpan {
    name: &'static str,
    started: Instant,
    #[cfg(target_os = "macos")]
    signpost: Option<(OSSignposter, OSSignpostInterval)>,
}

impl PerformanceSpan {
    #[must_use]
    pub fn new(name: &'static str, detail: impl AsRef<str>) -> Self {
        let detail = detail.as_ref();
        log::info!(
            target: "saytrace::performance",
            "event=begin span={name} {detail}"
        );

        #[cfg(target_os = "macos")]
        let signpost =
            OSSignposter::new("com.localtranscript.desktop", CATEGORY_POINTS_OF_INTEREST)
                .ok()
                .and_then(|signposter| {
                    if !signposter.is_enabled() {
                        return None;
                    }
                    let id = signposter.make_signpost_id();
                    let interval = signposter.begin_interval(name, id, detail);
                    Some((signposter, interval))
                });

        Self {
            name,
            started: Instant::now(),
            #[cfg(target_os = "macos")]
            signpost,
        }
    }
}

impl Drop for PerformanceSpan {
    fn drop(&mut self) {
        let duration_ms = self.started.elapsed().as_millis();
        log::info!(
            target: "saytrace::performance",
            "event=end span={} duration_ms={duration_ms}",
            self.name
        );

        #[cfg(target_os = "macos")]
        if let Some((signposter, interval)) = self.signpost.take() {
            signposter.end_interval(self.name, interval, &format!("duration_ms={duration_ms}"));
        }
    }
}
