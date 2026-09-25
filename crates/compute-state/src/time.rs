//! Timestamps FeltDB can order.
//!
//! FeltDB compares `datetime` values as strings. RFC 3339 as chrono writes
//! it by default drops trailing zero fractions (`…:05Z`, `…:05.1Z`,
//! `…:05.123456789Z`), and `.` sorts before `Z`, so string order is not
//! time order. Fields Compute asks FeltDB to order by are written with a
//! fixed nine-digit fraction instead, which sorts exactly as time does.
//! Reading accepts any RFC 3339, so records written before this format
//! still decode; among those, only values within the same second whose
//! fractions have different widths can order differently.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serializer};

/// A timestamp in the fixed-width form FeltDB orders correctly.
pub fn canonical(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub fn serialize<S: Serializer>(at: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&canonical(at))
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<DateTime<Utc>, D::Error> {
    DateTime::<Utc>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    #[test]
    fn canonical_timestamps_sort_as_time_does() {
        let whole = chrono::Utc.with_ymd_and_hms(2026, 9, 25, 12, 0, 5).unwrap();
        let later = whole + chrono::TimeDelta::milliseconds(100);
        let default = |at: &chrono::DateTime<chrono::Utc>| serde_json::to_string(at).unwrap();
        assert!(
            default(&whole) > default(&later),
            "the default form misorders"
        );
        assert!(super::canonical(&whole) < super::canonical(&later));
        assert_eq!(super::canonical(&whole), "2026-09-25T12:00:05.000000000Z");
    }
}
