use crate::errors::RecurrenceError;

/// Represents a recurrence specification, ported from tw's period matching.
#[derive(Debug, Clone, PartialEq)]
pub enum RecurrenceSpec {
    // Named periods
    Daily,
    Weekdays,
    Weekly,
    Biweekly,
    Monthly,
    Bimonthly,
    Quarterly,
    Semiannual,
    Annual,
    Biannual,
    // N-unit shorthand: "3m", "2q", "1y", "14d", "2w"
    NMonths(u32),
    NQuarters(u32),
    NYears(u32),
    NDays(u32),
    NWeeks(u32),
    // ISO 8601 durations: "P1M", "P3M", "P1Y", "P14D"
    IsoMonths(u32),
    IsoYears(u32),
    IsoDays(u32),
    IsoWeeks(u32),
    // Fallback: raw duration in seconds (tw's Duration parse)
    Seconds(i64),
}

/// Validate that an ISO-duration n-value is nonzero and return the spec.
fn nonzero_iso(
    n: u32,
    spec: RecurrenceSpec,
    input: &str,
) -> Result<RecurrenceSpec, RecurrenceError> {
    if n == 0 {
        Err(RecurrenceError::Parse(format!(
            "zero-period recurrence spec is invalid: {}",
            input
        )))
    } else {
        Ok(spec)
    }
}

/// Parse a recurrence spec string into a `RecurrenceSpec`.
///
/// Matching is case-sensitive to match tw behavior. Zero-period and
/// non-positive specs are rejected as they produce no-op schedules.
pub fn parse_spec(input: &str) -> Result<RecurrenceSpec, RecurrenceError> {
    if input.is_empty() {
        return Err(RecurrenceError::Parse("empty string".to_string()));
    }

    // Named periods — exact match, case-sensitive
    match input {
        "daily" => return Ok(RecurrenceSpec::Daily),
        "weekdays" => return Ok(RecurrenceSpec::Weekdays),
        "weekly" | "P1W" => return Ok(RecurrenceSpec::Weekly),
        "biweekly" | "P2W" => return Ok(RecurrenceSpec::Biweekly),
        "monthly" | "P1M" => return Ok(RecurrenceSpec::Monthly),
        "bimonthly" | "P2M" => return Ok(RecurrenceSpec::Bimonthly),
        "quarterly" | "P3M" => return Ok(RecurrenceSpec::Quarterly),
        "semiannual" | "P6M" => return Ok(RecurrenceSpec::Semiannual),
        "annual" | "yearly" | "P1Y" => return Ok(RecurrenceSpec::Annual),
        "biannual" | "biyearly" | "P2Y" => return Ok(RecurrenceSpec::Biannual),
        _ => {}
    }

    // ISO 8601 duration: "P<n>M", "P<n>Y", "P<n>D", "P<n>W"
    if let Some(rest) = input.strip_prefix('P') {
        if let Some(n_str) = rest.strip_suffix('M') {
            if let Ok(n) = n_str.parse::<u32>() {
                return nonzero_iso(n, RecurrenceSpec::IsoMonths(n), input);
            }
        } else if let Some(n_str) = rest.strip_suffix('Y') {
            if let Ok(n) = n_str.parse::<u32>() {
                return nonzero_iso(n, RecurrenceSpec::IsoYears(n), input);
            }
        } else if let Some(n_str) = rest.strip_suffix('D') {
            if let Ok(n) = n_str.parse::<u32>() {
                return nonzero_iso(n, RecurrenceSpec::IsoDays(n), input);
            }
        } else if let Some(n_str) = rest.strip_suffix('W') {
            if let Ok(n) = n_str.parse::<u32>() {
                return nonzero_iso(n, RecurrenceSpec::IsoWeeks(n), input);
            }
        }
        return Err(RecurrenceError::Parse(format!(
            "unrecognized ISO duration: {}",
            input
        )));
    }

    // Shorthand: "<n>m", "<n>q", "<n>y", "<n>d", "<n>w"
    // Only trigger when last char is a known unit letter and the prefix is a number.
    if input.len() >= 2 {
        let (n_str, unit) = input.split_at(input.len() - 1);
        if let Ok(n) = n_str.parse::<u32>() {
            let spec = match unit {
                "m" => Some(RecurrenceSpec::NMonths(n)),
                "q" => Some(RecurrenceSpec::NQuarters(n)),
                "y" => Some(RecurrenceSpec::NYears(n)),
                "d" => Some(RecurrenceSpec::NDays(n)),
                "w" => Some(RecurrenceSpec::NWeeks(n)),
                _ => None, // fall through to seconds fallback
            };
            if let Some(s) = spec {
                if n == 0 {
                    return Err(RecurrenceError::Parse(format!(
                        "zero-period recurrence spec is invalid: {}",
                        input
                    )));
                }
                return Ok(s);
            }
        }
    }

    // Fallback: try parse as seconds (raw integer; must be positive)
    if let Ok(n) = input.parse::<i64>() {
        if n <= 0 {
            return Err(RecurrenceError::Parse(format!(
                "recurrence duration must be positive, got: {}",
                n
            )));
        }
        return Ok(RecurrenceSpec::Seconds(n));
    }

    Err(RecurrenceError::Parse(format!(
        "unrecognized recurrence spec: {}",
        input
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn named_periods() {
        assert_eq!(parse_spec("daily").unwrap(), RecurrenceSpec::Daily);
        assert_eq!(parse_spec("weekdays").unwrap(), RecurrenceSpec::Weekdays);
        assert_eq!(parse_spec("weekly").unwrap(), RecurrenceSpec::Weekly);
        assert_eq!(parse_spec("biweekly").unwrap(), RecurrenceSpec::Biweekly);
        assert_eq!(parse_spec("monthly").unwrap(), RecurrenceSpec::Monthly);
        assert_eq!(parse_spec("bimonthly").unwrap(), RecurrenceSpec::Bimonthly);
        assert_eq!(parse_spec("quarterly").unwrap(), RecurrenceSpec::Quarterly);
        assert_eq!(
            parse_spec("semiannual").unwrap(),
            RecurrenceSpec::Semiannual
        );
        assert_eq!(parse_spec("annual").unwrap(), RecurrenceSpec::Annual);
        assert_eq!(parse_spec("yearly").unwrap(), RecurrenceSpec::Annual);
        assert_eq!(parse_spec("biannual").unwrap(), RecurrenceSpec::Biannual);
        assert_eq!(parse_spec("biyearly").unwrap(), RecurrenceSpec::Biannual);
    }

    #[test]
    fn iso_durations() {
        assert_eq!(parse_spec("P1W").unwrap(), RecurrenceSpec::Weekly);
        assert_eq!(parse_spec("P2W").unwrap(), RecurrenceSpec::Biweekly);
        assert_eq!(parse_spec("P1M").unwrap(), RecurrenceSpec::Monthly);
        assert_eq!(parse_spec("P2M").unwrap(), RecurrenceSpec::Bimonthly);
        assert_eq!(parse_spec("P3M").unwrap(), RecurrenceSpec::Quarterly);
        assert_eq!(parse_spec("P6M").unwrap(), RecurrenceSpec::Semiannual);
        assert_eq!(parse_spec("P1Y").unwrap(), RecurrenceSpec::Annual);
        assert_eq!(parse_spec("P2Y").unwrap(), RecurrenceSpec::Biannual);
        assert_eq!(parse_spec("P14D").unwrap(), RecurrenceSpec::IsoDays(14));
        assert_eq!(parse_spec("P4M").unwrap(), RecurrenceSpec::IsoMonths(4));
        assert_eq!(parse_spec("P5Y").unwrap(), RecurrenceSpec::IsoYears(5));
        assert_eq!(parse_spec("P7W").unwrap(), RecurrenceSpec::IsoWeeks(7));
    }

    #[test]
    fn shorthand_forms() {
        assert_eq!(parse_spec("1m").unwrap(), RecurrenceSpec::NMonths(1));
        assert_eq!(parse_spec("12m").unwrap(), RecurrenceSpec::NMonths(12));
        assert_eq!(parse_spec("2q").unwrap(), RecurrenceSpec::NQuarters(2));
        assert_eq!(parse_spec("1y").unwrap(), RecurrenceSpec::NYears(1));
        assert_eq!(parse_spec("7d").unwrap(), RecurrenceSpec::NDays(7));
        assert_eq!(parse_spec("2w").unwrap(), RecurrenceSpec::NWeeks(2));
    }

    #[test]
    fn fallback_seconds() {
        assert_eq!(parse_spec("86400").unwrap(), RecurrenceSpec::Seconds(86400));
        assert_eq!(parse_spec("3600").unwrap(), RecurrenceSpec::Seconds(3600));
    }

    #[test]
    fn errors() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("gibberish").is_err());
        assert!(parse_spec("Pxyz").is_err());
    }

    #[test]
    fn zero_period_rejected() {
        assert!(parse_spec("P0M").is_err());
        assert!(parse_spec("P0D").is_err());
        assert!(parse_spec("P0W").is_err());
        assert!(parse_spec("P0Y").is_err());
        assert!(parse_spec("0d").is_err());
        assert!(parse_spec("0m").is_err());
        assert!(parse_spec("0w").is_err());
    }

    #[test]
    fn non_positive_seconds_rejected() {
        assert!(parse_spec("0").is_err());
        assert!(parse_spec("-86400").is_err());
    }
}
