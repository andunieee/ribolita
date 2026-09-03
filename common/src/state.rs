use serde::de;

#[derive(PartialEq, Debug, Clone)]
pub struct State(pub rmpv::Value);

impl<'de> de::Deserialize<'de> for State {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> de::Visitor<'de> for ValueVisitor {
            type Value = rmpv::Value;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("any valid messagepack value")
            }

            #[inline]
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::Integer(v.into()))
            }

            #[inline]
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::Integer(v.into()))
            }

            #[inline]
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::F64(v))
            }

            #[inline]
            fn visit_f32<E>(self, v: f32) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::F32(v))
            }

            #[inline]
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::String(v.into()))
            }

            #[inline]
            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::Boolean(v))
            }

            #[inline]
            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::Nil)
            }

            #[inline]
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::Binary(v.to_owned()))
            }

            #[inline]
            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(rmpv::Value::Nil)
            }

            #[inline]
            fn visit_seq<A>(self, mut v: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut seq = Vec::with_capacity(v.size_hint().unwrap_or(256));
                while let Some(elem) = v.next_element()? {
                    seq.push(elem);
                }
                Ok(rmpv::Value::Array(seq))
            }

            fn visit_map<A>(self, mut v: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut map = Vec::with_capacity(v.size_hint().unwrap_or(256));
                while let Some((key, value)) = v.next_entry()? {
                    map.push((key, value));
                }
                Ok(rmpv::Value::Map(map))
            }
        }

        deserializer.deserialize_any(ValueVisitor).map(State)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        key::{self, KeyPart},
        state::State,
        value::from_rows,
    };

    fn encode_value(value: rmpv::ValueRef<'_>) -> Vec<u8> {
        let mut res = Vec::with_capacity(128);
        rmpv::encode::write_value_ref(&mut res, &value).unwrap();
        res
    }

    #[test]
    fn test_state_value() {
        let initial = [
            (
                key::to_bytes(&[KeyPart::Field("hello")]),
                encode_value(rmpv::ValueRef::String("world".into())),
            ),
            (
                key::to_bytes(&[KeyPart::Field("goodbye")]),
                encode_value(rmpv::ValueRef::String("world".into())),
            ),
        ];

        let source: Vec<(&[u8], &[u8])> = initial
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();

        let s_value: State = from_rows(&source).unwrap();
        assert_eq!(
            s_value,
            State(rmpv::Value::Map(vec![
                (
                    rmpv::Value::String("hello".into()),
                    rmpv::Value::String("world".into())
                ),
                (
                    rmpv::Value::String("goodbye".into()),
                    rmpv::Value::String("world".into())
                )
            ]))
        );
    }
}
