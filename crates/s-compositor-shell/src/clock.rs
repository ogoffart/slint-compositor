//! Clock formatting helpers.

use chrono::{DateTime, Local, Timelike};

/// Format a timestamp for the panel clock as a stacked "HH\nMM" string so it
/// reads well in the narrow vertical right-edge panel.
pub fn format_clock(now: DateTime<Local>) -> String {
    format!("{:02}\n{:02}", now.hour(), now.minute())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn formats_zero_padded_stacked() {
        let t = Local.with_ymd_and_hms(2026, 6, 3, 9, 5, 0).unwrap();
        assert_eq!(format_clock(t), "09\n05");
    }
}
