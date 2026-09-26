//! Durations written as `"30m"`, `"120s"`, `"1h 30m"`, or a bare number of
//! seconds. `0` / `"0"` means zero (used to disable idling).

use serde::{Deserialize, Deserializer, de};
use std::time::Duration;

pub fn parse(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    humantime::parse_duration(s).map_err(|e| format!("invalid duration `{s}`: {e}"))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Raw {
    Secs(u64),
    Text(String),
}

pub fn deserialize_opt<'de, D>(d: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<Raw>::deserialize(d)? {
        None => Ok(None),
        Some(Raw::Secs(s)) => Ok(Some(Duration::from_secs(s))),
        Some(Raw::Text(t)) => parse(&t).map(Some).map_err(de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse("0").unwrap(), Duration::ZERO);
        assert_eq!(parse("1h 30m").unwrap(), Duration::from_secs(5400));
        assert!(parse("soon").is_err());
    }
}
