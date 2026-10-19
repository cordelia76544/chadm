//! TOML integers are signed 64-bit; native ZFS GUIDs span the full u64 range.
//! Write decimal strings, while accepting legacy nonnegative integer metadata.
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<S: Serializer>(guid: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
    guid.map(|value| value.to_string()).serialize(serializer)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Guid {
        Integer(u64),
        Decimal(String),
    }

    Option::<Guid>::deserialize(deserializer)?
        .map(|value| match value {
            Guid::Integer(value) => Ok(value),
            Guid::Decimal(value) => {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(serde::de::Error::custom(
                        "GUID must be a decimal u64 string",
                    ));
                }
                value.parse().map_err(|_| {
                    serde::de::Error::custom("GUID is outside the unsigned 64-bit range")
                })
            }
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Metadata {
        #[serde(default, skip_serializing_if = "Option::is_none", with = "super")]
        guid: Option<u64>,
    }

    #[test]
    fn full_unsigned_range_round_trips_as_decimal_strings() {
        for value in [0, 1, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
            let metadata = Metadata { guid: Some(value) };
            let encoded = toml::to_string(&metadata).unwrap();
            assert_eq!(encoded, format!("guid = \"{value}\"\n"));
            assert_eq!(toml::from_str::<Metadata>(&encoded).unwrap(), metadata);
        }
    }

    #[test]
    fn legacy_integers_and_absent_guids_remain_readable() {
        for value in [0, 1, i64::MAX as u64] {
            let metadata: Metadata = toml::from_str(&format!("guid = {value}\n")).unwrap();
            assert_eq!(metadata.guid, Some(value));
        }
        let metadata: Metadata = toml::from_str("").unwrap();
        assert_eq!(metadata.guid, None);
        assert_eq!(toml::to_string(&metadata).unwrap(), "");
    }

    #[test]
    fn invalid_guids_are_rejected_without_wrapping_or_truncation() {
        for value in [
            "-1",
            "1.0",
            "true",
            "[]",
            "{}",
            "\"\"",
            "\"-1\"",
            "\"+1\"",
            "\" 1\"",
            "\"1 \"",
            "\"1e2\"",
            "\"0xff\"",
            "\"１２\"",
            "\"18446744073709551616\"",
        ] {
            assert!(
                toml::from_str::<Metadata>(&format!("guid = {value}\n")).is_err(),
                "{value}"
            );
        }
    }
}
