use serde_json;

pub fn json_value_to_rmp_value(jval: &serde_json::Value) -> rmpv::Value {
    match jval {
        serde_json::Value::Bool(b) => rmpv::Value::Boolean(*b),
        serde_json::Value::Null => rmpv::Value::Nil,
        serde_json::Value::String(s) => rmpv::Value::String(s.as_str().into()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rmpv::Value::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                rmpv::Value::Integer(u.into())
            } else if let Some(f) = n.as_f64() {
                rmpv::Value::F64(f)
            } else {
                0.into()
            }
        }
        serde_json::Value::Array(arr) => {
            rmpv::Value::Array(arr.iter().map(json_value_to_rmp_value).collect())
        }
        serde_json::Value::Object(map) => rmpv::Value::Map(
            map.iter()
                .map(|(k, v)| {
                    (
                        rmpv::Value::String(k.as_str().into()),
                        json_value_to_rmp_value(v),
                    )
                })
                .collect(),
        ),
    }
}

pub fn rmp_value_to_json_value(mpval: &rmpv::Value) -> serde_json::Value {
    match mpval {
        rmpv::Value::Boolean(b) => serde_json::Value::Bool(*b),
        rmpv::Value::Nil => serde_json::Value::Null,
        rmpv::Value::String(s) => serde_json::Value::String(s.as_str().unwrap_or("").to_string()),
        rmpv::Value::Integer(i) => {
            if let Some(i64_val) = i.as_i64() {
                serde_json::Value::Number(serde_json::Number::from(i64_val))
            } else if let Some(u64_val) = i.as_u64() {
                serde_json::Value::Number(serde_json::Number::from(u64_val))
            } else {
                serde_json::Value::Number(serde_json::Number::from(0))
            }
        }
        rmpv::Value::F64(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f).unwrap_or(serde_json::Number::from(0)),
        ),
        rmpv::Value::F32(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f as f64).unwrap_or(serde_json::Number::from(0)),
        ),
        rmpv::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(rmp_value_to_json_value).collect())
        }
        rmpv::Value::Map(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                if let rmpv::Value::String(key_str) = k {
                    obj.insert(
                        key_str.as_str().unwrap_or("").to_string(),
                        rmp_value_to_json_value(v),
                    );
                }
            }
            serde_json::Value::Object(obj)
        }
        rmpv::Value::Binary(_) => serde_json::Value::Null, // or handle as string?
        rmpv::Value::Ext(_, _) => serde_json::Value::Null, // not handling extensions
    }
}
