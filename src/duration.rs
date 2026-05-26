use std::time::Duration;

use serde::{Deserialize, Deserializer, de};

/// Parse a human-readable duration string into a [`Duration`].
///
/// Format: `<number><unit>` — units are:
/// - `ms` — milliseconds
/// - `s`  — seconds
/// - `m`  — minutes
/// - `h`  — hours
/// - `d`  — days
/// - `w`  — weeks
/// - `mo` — months (30 days)
pub fn parse(s: &str) -> Result<Duration, String> {
    let s = s.trim();

    let split = s
        .find(|c: char| c.is_alphabetic())
        .ok_or_else(|| format!("missing unit in duration {:?}", s))?;

    let (num_str, unit) = s.split_at(split);
    let n: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid number {:?} in duration {:?}", num_str, s))?;

    let dur = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        "d" => Duration::from_secs(n * 86_400),
        "w" => Duration::from_secs(n * 604_800),
        "mo" => Duration::from_secs(n * 2_592_000),
        other => return Err(format!("unknown duration unit {:?}", other)),
    };

    Ok(dur)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    parse(&s).map_err(de::Error::custom)
}

pub mod option {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        s.map(|s| super::parse(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}
