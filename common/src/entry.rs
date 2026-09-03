use crate::rmp_json;
use serde::{Deserialize, Serialize};
use serde_json;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub when: u64,
    pub kind: String,
    pub values: Vec<(String, rmpv::Value)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryValueOutput {
    pub key: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryOutput {
    pub id: u64,
    pub when: u64,
    pub kind: String,
    pub values: Vec<EntryValueOutput>,
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
        id: entry.id,
        when: entry.when,
        kind: entry.kind,
        values,
    }
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Entry #{} [{}]", self.id, self.kind)?;
        writeln!(f, "  timestamp: {}", self.when)?;

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
            id: 1,
            when: 12345,
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
}
