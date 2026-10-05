//! JSON as Go's encoding/json decodes it into a struct, for a seccomp profile: objects
//! kept in their order, a key matched to a field exactly or else regardless of case, a
//! later key overwriting an earlier one, `null` leaving a field as it was, and unknown
//! keys ignored.

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Json;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
                Ok(Json::Bool(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
                Ok(Json::Number(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
                Ok(Json::Number(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
                serde_json::Number::from_f64(v)
                    .map(Json::Number)
                    .ok_or_else(|| E::custom("a number JSON cannot hold"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
                Ok(Json::String(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
                Ok(Json::String(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Json, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element()? {
                    out.push(v);
                }
                Ok(Json::Array(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Json, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = a.next_entry::<String, Json>()? {
                    out.push((k, v));
                }
                Ok(Json::Object(out))
            }
        }
        d.deserialize_any(V)
    }
}

impl Json {
    pub fn parse(bytes: &[u8]) -> Result<Json, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    /// The keys Go's decoder sets field `name` from, in order: each matching it exactly
    /// or regardless of ASCII case (encoding/json's foldName).
    fn matches<'a, 'b>(&'a self, name: &'b str) -> impl DoubleEndedIterator<Item = &'a Json> + use<'a, 'b> {
        let entries: &[(String, Json)] = match self {
            Json::Object(e) => e,
            _ => &[],
        };
        entries
            .iter()
            .filter(move |(k, _)| k == name || k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    /// A slice or pointer field: the last key wins, and a `null` there leaves it nil.
    pub fn field<'a>(&'a self, name: &str) -> Option<&'a Json> {
        self.matches(name)
            .next_back()
            .filter(|v| !matches!(v, Json::Null))
    }

    /// A string or number field: a `null` has no effect, so the last key that is not
    /// one wins.
    pub fn value<'a>(&'a self, name: &str) -> Option<&'a Json> {
        self.matches(name).rev().find(|v| !matches!(v, Json::Null))
    }

    /// Go's name for its kind, as an UnmarshalTypeError says it.
    pub fn kind(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "bool",
            Json::Number(_) => "number",
            Json::String(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
}
