//! JSON encodings used by the lightwallet API: `uint64-string`, hex binary,
//! and the request bodies. Unknown request fields are ignored so newer
//! clients keep working; out-of-range numbers are rejected.

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// A u64 sent as a decimal string, because JavaScript numbers lose precision
/// past 2^53. Accepts a JSON number too, for lenient clients.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct U64Str(pub u64);

impl Serialize for U64Str {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for U64Str {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = U64Str;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an unsigned 64-bit integer, as a string or number")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<U64Str, E> {
                Ok(U64Str(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<U64Str, E> {
                u64::try_from(v).map(U64Str).map_err(|_| E::custom("negative value"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<U64Str, E> {
                if v.is_empty() || v.len() > 20 || !v.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(E::custom("not a decimal u64"));
                }
                v.parse().map(U64Str).map_err(|_| E::custom("u64 out of range"))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
pub struct Credentials {
    pub address: String,
    pub view_key: String,
}

#[derive(Deserialize)]
pub struct HistoryRequest {
    #[serde(flatten)]
    pub creds: Credentials,
    #[serde(default)]
    pub min_height: Option<u64>,
    #[serde(default)]
    pub max_count: Option<u64>,
}

#[derive(Deserialize)]
pub struct UnspentRequest {
    #[serde(flatten)]
    pub creds: Credentials,
    pub amount: U64Str,
    #[serde(default)]
    pub mixin: Option<u32>,
    #[serde(default)]
    pub use_dust: Option<bool>,
    #[serde(default)]
    pub dust_threshold: Option<U64Str>,
    #[serde(default)]
    pub min_height: Option<u64>,
    #[serde(default)]
    pub max_count: Option<u64>,
    #[serde(default)]
    pub token_id: Option<String>,
    #[serde(default)]
    pub all_tokens: Option<bool>,
}

#[derive(Deserialize)]
pub struct RandomOutsRequest {
    pub count: u32,
    pub amounts: Vec<U64Str>,
    #[serde(default)]
    pub token_ids: Vec<String>,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    #[serde(flatten)]
    pub creds: Credentials,
    #[serde(default)]
    pub create_account: bool,
    #[serde(default)]
    pub generated_locally: bool,
}

#[derive(Deserialize)]
pub struct SubmitRequest {
    pub tx: String,
    #[serde(default)]
    pub fee: Option<String>,
}

#[derive(Deserialize)]
pub struct TokenBalancesRequest {
    #[serde(flatten)]
    pub creds: Credentials,
    #[serde(default)]
    pub token_ids: Vec<String>,
}

#[derive(Deserialize)]
pub struct TokenInfoRequest {
    pub token_id: String,
}

#[derive(Deserialize)]
pub struct TokenListRequest {
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_token_count")]
    pub count: u64,
}

fn default_token_count() -> u64 {
    100
}

pub fn hex32(text: &str) -> Option<[u8; 32]> {
    lws_core::parse_key(text)
}

pub fn is_token_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_strings() {
        let v: U64Str = serde_json::from_str("\"18446744073709551615\"").unwrap();
        assert_eq!(v.0, u64::MAX);
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"18446744073709551615\"");
        assert_eq!(serde_json::from_str::<U64Str>("12").unwrap().0, 12);
        for bad in ["\"18446744073709551616\"", "\"-1\"", "\"1e3\"", "-1", "\"\"", "1.5"] {
            assert!(serde_json::from_str::<U64Str>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn requests_ignore_unknown_fields() {
        let r: HistoryRequest =
            serde_json::from_str(r#"{"address":"a","view_key":"b","min_height":5,"future_field":true}"#).unwrap();
        assert_eq!((r.creds.address.as_str(), r.min_height, r.max_count), ("a", Some(5), None));
        assert!(serde_json::from_str::<HistoryRequest>(r#"{"address":"a","view_key":"b","min_height":-5}"#).is_err());
    }
}
