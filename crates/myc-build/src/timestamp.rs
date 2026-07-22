//! Build timestamps with reproducibility support.
//!
//! `created` is part of the canonical manifest, so an uncontrolled "now"
//! breaks build determinism. Resolution order:
//!
//! 1. explicit value (`myc build --timestamp <rfc3339|unix>`)
//! 2. the `SOURCE_DATE_EPOCH` environment variable (reproducible-builds.org)
//! 3. the current time

use std::time::{SystemTime, UNIX_EPOCH};

/// Resolve the build timestamp (RFC 3339).
pub fn resolve(explicit: Option<&str>) -> Result<String, String> {
    if let Some(t) = explicit {
        return parse(t);
    }
    if let Ok(sde) = std::env::var("SOURCE_DATE_EPOCH") {
        return parse(&sde).map_err(|e| format!("SOURCE_DATE_EPOCH: {e}"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(unix_to_rfc3339(now))
}

/// Accept either a unix timestamp (`1700000000`) or an RFC 3339 string.
pub fn parse(input: &str) -> Result<String, String> {
    if let Ok(secs) = input.parse::<i64>() {
        return Ok(unix_to_rfc3339(secs));
    }
    // Light RFC 3339 shape check: `YYYY-MM-DDTHH:MM:SS` prefix.
    let b = input.as_bytes();
    let shaped = b.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b't')
        && b[13] == b':'
        && b[16] == b':';
    if shaped {
        Ok(input.to_string())
    } else {
        Err(format!(
            "'{input}' is neither a unix timestamp nor an RFC 3339 date"
        ))
    }
}

/// Format a unix timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn unix_to_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_conversion() {
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(unix_to_rfc3339(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(unix_to_rfc3339(951_782_400), "2000-02-29T00:00:00Z"); // leap day
    }

    #[test]
    fn parse_accepts_unix_and_rfc3339() {
        assert_eq!(parse("0").unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            parse("2026-01-01T00:00:00Z").unwrap(),
            "2026-01-01T00:00:00Z"
        );
        assert!(parse("yesterday").is_err());
    }
}
