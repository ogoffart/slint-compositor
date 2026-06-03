//! Month-grid maths for the panel clock's calendar popover.

use chrono::{DateTime, Datelike, Local, NaiveDate};

/// One cell of the month grid. `day == 0` is a filler (leading/trailing blank).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub day: u32,
    pub today: bool,
}

/// Build the calendar title (e.g. "June 2026") and a 6x7 grid of day cells for
/// the month containing `now`. Weeks start on Monday.
pub fn month_grid(now: DateTime<Local>) -> (String, Vec<Cell>) {
    let (year, month, today) = (now.year(), now.month(), now.day());
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid first-of-month");
    let lead = first.weekday().num_days_from_monday() as usize;
    let days = days_in_month(year, month);

    let mut cells = Vec::with_capacity(42);
    for _ in 0..lead {
        cells.push(Cell {
            day: 0,
            today: false,
        });
    }
    for d in 1..=days {
        cells.push(Cell {
            day: d,
            today: d == today,
        });
    }
    while cells.len() < 42 {
        cells.push(Cell {
            day: 0,
            today: false,
        });
    }

    let title = format!("{} {}", month_name(month), year);
    (title, cells)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next_first = NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid next-month");
    next_first.pred_opt().expect("day before next month").day()
}

fn month_name(month: u32) -> &'static str {
    [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ]
    .get((month - 1) as usize)
    .copied()
    .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn grid_shape_and_today() {
        // 1 June 2026 is a Monday, so there is no leading filler.
        let now = Local.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        let (title, cells) = month_grid(now);
        assert_eq!(title, "June 2026");
        assert_eq!(cells.len(), 42);
        assert_eq!(
            cells[0],
            Cell {
                day: 1,
                today: false
            }
        );
        // 30 days in June, all present.
        let real: Vec<u32> = cells.iter().filter(|c| c.day != 0).map(|c| c.day).collect();
        assert_eq!(real.len(), 30);
        assert_eq!(*real.last().unwrap(), 30);
        // The 15th is flagged today.
        assert!(cells.iter().any(|c| c.day == 15 && c.today));
    }

    #[test]
    fn leading_filler_for_midweek_start() {
        // 1 January 2026 is a Thursday -> 3 leading blanks (Mon,Tue,Wed).
        let now = Local.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        let (_, cells) = month_grid(now);
        assert_eq!(cells[0].day, 0);
        assert_eq!(
            cells[3],
            Cell {
                day: 1,
                today: true
            }
        );
        assert_eq!(cells.iter().filter(|c| c.day != 0).count(), 31);
    }
}
