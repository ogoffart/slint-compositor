//! Clock formatting helpers.

use chrono::{DateTime, Local, Timelike};

/// Format a timestamp for the panel clock, returning the zero-padded hours and
/// minutes separately. The panel shows them as "HH:MM" on one line when there
/// is room, and stacks the hours over the minutes when it is too narrow.
pub fn format_clock(now: DateTime<Local>) -> (String, String) {
    (format!("{:02}", now.hour()), format!("{:02}", now.minute()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn formats_zero_padded_parts() {
        let t = Local.with_ymd_and_hms(2026, 6, 3, 9, 5, 0).unwrap();
        assert_eq!(format_clock(t), ("09".to_string(), "05".to_string()));
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn formats_afternoon_24h() {
        let t = Local.with_ymd_and_hms(2026, 6, 3, 15, 36, 0).unwrap();
        assert_eq!(format_clock(t), ("15".to_string(), "36".to_string()));
    }

    #[test]
    fn formats_midnight() {
        let t = Local.with_ymd_and_hms(2026, 6, 3, 0, 0, 0).unwrap();
        assert_eq!(format_clock(t), ("00".to_string(), "00".to_string()));
    }
}
