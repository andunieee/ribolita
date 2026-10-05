use crate::rmp_json;
use serde::{Deserialize, Serialize};
use serde_json;

/// Identifies the device (or service) that created an entry, e.g. an ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Author(pub [u8; 32]);

impl Author {
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }
}

impl std::fmt::Debug for Author {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Author({})", &self.to_hex()[..10])
    }
}

impl std::fmt::Display for Author {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::str::FromStr for Author {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = from_hex(s).ok_or(ParseIdError)?;
        Ok(Author(bytes.try_into().map_err(|_| ParseIdError)?))
    }
}

impl Serialize for Author {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Author {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;

        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Author;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("32 bytes")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Author, E> {
                Ok(Author(
                    v.try_into()
                        .map_err(|_| E::invalid_length(v.len(), &self))?,
                ))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Author, A::Error> {
                let mut bytes = [0u8; 32];
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(Author(bytes))
            }
        }

        deserializer.deserialize_bytes(BytesVisitor)
    }
}

/// Globally unique id of an entry: a v1 cuid, which sorts by creation time. It says when the
/// entry was created, which is only relevant for syncing; when the event happened is
/// [`Entry::when`].
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub String);

impl EntryId {
    /// below every id
    pub const MIN: &'static str = "";
    /// above every id (cuids are lowercase alphanumeric)
    pub const MAX: &'static str = "~";

    #[cfg(feature = "ids")]
    pub fn generate() -> EntryId {
        EntryId(cuid::cuid1())
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<EntryId> {
        std::str::from_utf8(bytes)
            .ok()
            .map(|s| EntryId(s.to_string()))
    }
}

impl std::fmt::Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for EntryId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = s.len() >= 2
            && s.len() <= 32
            && s.starts_with('c')
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if !valid {
            return Err(ParseIdError);
        }
        Ok(EntryId(s.to_string()))
    }
}

#[derive(thiserror::Error, Debug)]
#[error("invalid entry id, expected a cuid")]
pub struct ParseIdError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    /// when the event happened, as reported (seconds since the unix epoch). Entries are applied
    /// in `(when, id)` order.
    pub when: u64,
    pub author: Author,
    pub kind: String,
    pub values: Vec<(String, rmpv::Value)>,
    /// set on tombstones: this entry deletes the entry with that id, and has no other effect
    pub deletes: Option<EntryId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryValueOutput {
    pub key: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryOutput {
    pub id: String,
    pub when: u64,
    pub author: String,
    pub kind: String,
    pub values: Vec<EntryValueOutput>,
    pub deletes: Option<String>,
}

pub fn entry_to_json(entry: Entry) -> EntryOutput {
    let values = entry
        .values
        .into_iter()
        .map(|(key, value)| EntryValueOutput {
            key,
            value: rmp_json::rmp_value_to_json_value(&value),
        })
        .collect();

    EntryOutput {
        id: entry.id.0,
        when: entry.when,
        author: entry.author.to_hex(),
        kind: entry.kind,
        values,
        deletes: entry.deletes.map(|id| id.0),
    }
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(target) = &self.deletes {
            return writeln!(f, "Entry {} deletes {}", self.id, target);
        }
        writeln!(f, "Entry {} [{}] at {}", self.id, self.kind, self.when)?;

        if self.values.is_empty() {
            writeln!(f, "  values: (none)")?;
        } else {
            writeln!(f, "  values:")?;

            // Calculate the maximum key length for alignment
            let max_key_len = self.values.iter().map(|(k, _)| k.len()).max().unwrap_or(0);

            for (key, value) in &self.values {
                writeln!(f, "    {:width$} = {}", key, value, width = max_key_len)?;
            }
        }

        Ok(())
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

// trait to define conversion from rmpv::Value
pub trait FromValue {
    fn from_value(value: &rmpv::Value) -> Option<Self>
    where
        Self: Sized;
}

// implementations for basic types
impl FromValue for bool {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }
}

impl FromValue for i64 {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Integer(i) => i.as_i64(),
            _ => None,
        }
    }
}

impl FromValue for u64 {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Integer(i) => i.as_u64(),
            _ => None,
        }
    }
}

impl FromValue for i32 {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Integer(i) => i.as_i64().and_then(|v| v.try_into().ok()),
            _ => None,
        }
    }
}

impl FromValue for u32 {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Integer(i) => i.as_u64().and_then(|v| v.try_into().ok()),
            _ => None,
        }
    }
}

impl FromValue for f64 {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::F32(f) => Some(*f as f64),
            rmpv::Value::F64(f) => Some(*f),
            rmpv::Value::Integer(i) => i.as_f64(),
            _ => None,
        }
    }
}

impl FromValue for f32 {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::F32(f) => Some(*f),
            rmpv::Value::F64(f) => Some(*f as f32),
            rmpv::Value::Integer(i) => i.as_f64().map(|v| v as f32),
            _ => None,
        }
    }
}

impl FromValue for String {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::String(s) => Some(s.as_str().unwrap_or("").to_string()),
            _ => None,
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Binary(b) => Some(b.clone()),
            _ => None,
        }
    }
}

// Implementation for Vec<T> where T: FromValue
impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Array(arr) => {
                let mut result = Vec::with_capacity(arr.len());
                for item in arr {
                    if let Some(converted) = T::from_value(item) {
                        result.push(converted);
                    } else {
                        return None; // If any item fails to convert, return None
                    }
                }
                Some(result)
            }
            _ => None,
        }
    }
}

// Implementation for Option<T> where T: FromValue
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &rmpv::Value) -> Option<Self> {
        match value {
            rmpv::Value::Nil => Some(None),
            _ => T::from_value(value).map(Some).or(Some(None)),
        }
    }
}

impl Entry {
    pub fn get_value<R: FromValue>(&self, name: &str) -> Option<R> {
        for (k, v) in &self.values {
            if k == name {
                return R::from_value(v);
            }
        }
        None
    }

    // Helper method to get the raw rmpv::Value if needed
    pub fn get_raw_value(&self, name: &str) -> Option<&rmpv::Value> {
        for (k, v) in &self.values {
            if k == name {
                return Some(v);
            }
        }
        None
    }

    // Convenience methods for common operations
    pub fn get_string(&self, name: &str) -> Option<String> {
        self.get_value::<String>(name)
    }

    pub fn get_i64(&self, name: &str) -> Option<i64> {
        self.get_value::<i64>(name)
    }

    pub fn get_bool(&self, name: &str) -> Option<bool> {
        self.get_value::<bool>(name)
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.get_value::<f64>(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    #[test]
    fn test_get_value() {
        let entry = Entry {
            id: EntryId("c1".into()),
            when: 12345,
            author: Author::default(),
            deletes: None,
            kind: "test".to_string(),
            values: vec![
                ("name".to_string(), Value::String("Alice".into())),
                ("age".to_string(), Value::Integer(30.into())),
                ("height".to_string(), Value::F64(5.8)),
                ("active".to_string(), Value::Boolean(true)),
                (
                    "scores".to_string(),
                    Value::Array(vec![
                        Value::Integer(85.into()),
                        Value::Integer(92.into()),
                        Value::Integer(78.into()),
                    ]),
                ),
                ("data".to_string(), Value::Binary(vec![1, 2, 3, 4])),
                ("empty".to_string(), Value::Nil),
            ],
        };

        // Test string extraction
        let name: Option<String> = entry.get_value("name");
        assert_eq!(name, Some("Alice".to_string()));

        // Test integer extraction
        let age: Option<i64> = entry.get_value("age");
        assert_eq!(age, Some(30));

        // Test float extraction
        let height: Option<f64> = entry.get_value("height");
        assert_eq!(height, Some(5.8));

        // Test boolean extraction
        let active: Option<bool> = entry.get_value("active");
        assert_eq!(active, Some(true));

        // Test array extraction
        let scores: Option<Vec<i64>> = entry.get_value("scores");
        assert_eq!(scores, Some(vec![85, 92, 78]));

        // Test binary data extraction
        let data: Option<Vec<u8>> = entry.get_value("data");
        assert_eq!(data, Some(vec![1, 2, 3, 4]));

        // Test Option<T> extraction for nil values
        let empty: Option<Option<String>> = entry.get_value("empty");
        assert_eq!(empty, Some(None));

        // Test non-existent key
        let missing: Option<String> = entry.get_value("missing");
        assert_eq!(missing, None);

        // Test type mismatch
        let wrong_type: Option<bool> = entry.get_value("name");
        assert_eq!(wrong_type, None);

        // Test convenience methods
        assert_eq!(entry.get_string("name"), Some("Alice".to_string()));
        assert_eq!(entry.get_i64("age"), Some(30));
        assert_eq!(entry.get_bool("active"), Some(true));
        assert_eq!(entry.get_f64("height"), Some(5.8));
    }

    #[test]
    fn test_entry_id() {
        assert!("ckabc123".parse::<EntryId>().is_ok());
        assert!("Xabc".parse::<EntryId>().is_err());
        assert!(EntryId::MIN < "c0" && "czzzzzzzz" < EntryId::MAX);

        let entry = Entry {
            id: EntryId("cabc".into()),
            when: 1,
            author: Author([7; 32]),
            kind: "k".into(),
            values: vec![],
            deletes: Some(EntryId("cdef".into())),
        };
        let bytes = rmp_serde::to_vec(&entry).unwrap();
        assert_eq!(rmp_serde::from_slice::<Entry>(&bytes).unwrap(), entry);
    }
}
