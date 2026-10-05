use crate::key::{self, KeyPart};
use crate::{KEY_LEN_BYTES, ROW_COUNT_BYTES, VAL_LEN_BYTES};
use serde::{de, ser};

#[derive(thiserror::Error, Debug, serde::Deserialize)]
pub enum Error {
    #[error("expected map at '{0}'")]
    ExpectedMap(String),

    #[error("expected sequence")]
    ExpectedSequence,

    #[error("there are no more rows to read")]
    EndOfRows,

    #[error("custom: {0}")]
    Custom(String),

    #[error("messagepack number doesn't fit")]
    MessagePackNumber,

    #[error("can't serialize complex tuples")]
    MessagePackTuple,

    #[error("messagepack failed to decode an invalid string")]
    MessagePackInvalidString,

    #[error("messagepack unexpected value")]
    MessagePackUnepectedValue,

    #[error("expected key index")]
    ExpectedKeyIndex,

    #[error("expected key field")]
    ExpectedKeyField,

    #[error("map keys must be strings")]
    MapKeyNotString,

    #[error("can't serialize unsupported")]
    UnsupportedValue,

    #[error("failed to decode key: {0}")]
    DecodeKey(#[from] crate::key::DecodeError),

    #[error("failed to decode rmpv value")]
    DecodeValue,

    #[error("failed to encode rmpv value")]
    EncodeValue,

    #[error("invalid input bytes")]
    InvalidBytes,
}

impl serde::de::Error for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self::Custom(msg.to_string())
    }
}

impl serde::ser::Error for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self::Custom(msg.to_string())
    }
}

struct ValueDeserializer<'de: 'b, 'b> {
    data: &'b [(&'de [u8], &'de [u8])],
    item_i: &'b mut usize,
    path_i: &'b mut [usize],
}

impl<'de: 'b, 'b> de::Deserializer<'de> for ValueDeserializer<'de, 'b> {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        if *self.item_i >= self.data.len() {
            return Err(Error::EndOfRows);
        }

        let mut path_i_tmp = self.path_i[*self.item_i];
        let (path, value) = &self.data[*self.item_i];
        if path.len() == path_i_tmp {
            // we've reached the end of the path
            match rmpv::decode::read_value_ref(&mut std::io::Cursor::new(*value))
                .map_err(|_| Error::DecodeValue)?
            {
                rmpv::ValueRef::Nil => visitor.visit_unit(),
                rmpv::ValueRef::Boolean(b) => visitor.visit_bool(b),
                rmpv::ValueRef::Integer(n) => {
                    if let Some(i) = n.as_i64() {
                        visitor.visit_i64(i)
                    } else {
                        visitor.visit_u64(n.as_u64().ok_or(Error::MessagePackNumber)?)
                    }
                }
                rmpv::ValueRef::String(s) => {
                    visitor.visit_borrowed_str(s.into_str().ok_or(Error::MessagePackInvalidString)?)
                }
                rmpv::ValueRef::F32(f) => visitor.visit_f32(f),
                rmpv::ValueRef::F64(f) => visitor.visit_f64(f),
                _ => Err(Error::MessagePackUnepectedValue),
            }
        } else {
            match key::decode_part(path, &mut path_i_tmp)? {
                KeyPart::Field(_) => self.deserialize_map(visitor),
                KeyPart::Index(_) => self.deserialize_seq(visitor),
            }
        }
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        match rmpv::decode::read_value_ref(&mut std::io::Cursor::new(self.data[*self.item_i].1))
            .map_err(|_| Error::DecodeValue)?
        {
            rmpv::ValueRef::Nil => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        if *self.item_i >= self.data.len() {
            return Err(Error::EndOfRows);
        }

        let (first, _) = &self.data[*self.item_i];
        let initial_path_i = self.path_i[*self.item_i];
        let mut peek_path_i = self.path_i[*self.item_i]; // only exists to be passed to decode_part(), then discarded
        match key::decode_part(first, &mut peek_path_i)? {
            KeyPart::Index(_) => Err(Error::ExpectedKeyField),
            KeyPart::Field(_) => {
                // figure out in which row the map ends (by checking where the prefixes cease to be equal)
                let mut item_i_map_end = *self.item_i;
                for (i, (path, _)) in self.data.iter().enumerate().skip(*self.item_i) {
                    if path.len() > initial_path_i
                        && path[..initial_path_i] == first[..initial_path_i]
                    {
                        // adjust this so all members of this map are in the same position
                        self.path_i[i] = initial_path_i;

                        item_i_map_end += 1; // mark the row where the map ends
                    } else {
                        break;
                    }
                }

                visitor.visit_map(MapDeserializer {
                    data: self.data,
                    item_i: self.item_i,
                    path_i: self.path_i,
                    item_i_map_end,
                })
            }
        }
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        if *self.item_i >= self.data.len() {
            return Err(Error::EndOfRows);
        }

        let (first, _) = self.data[*self.item_i];
        let initial_first_path_i = self.path_i[*self.item_i];
        match key::decode_part(first, &mut self.path_i[*self.item_i])? {
            KeyPart::Field(_) => Err(Error::ExpectedKeyIndex),
            KeyPart::Index(_) => {
                // check where the sequence ends (checking where the prefixes cease to be equal)
                // (but unlike the map this time we have to ignore the numeric index part)
                let mut item_i_seq_end = *self.item_i;
                for (path, _) in self.data.iter().skip(*self.item_i) {
                    if path.len() > initial_first_path_i
                        && path[..initial_first_path_i] == first[..initial_first_path_i]
                    {
                        // also advance the path index so we don't consider this numeric index in the
                        // next key or value extraction step
                        self.path_i[item_i_seq_end] = self.path_i[*self.item_i];
                        item_i_seq_end += 1;
                    } else {
                        break;
                    }
                }

                visitor.visit_seq(SeqDeserializer {
                    data: self.data,
                    item_i: self.item_i,
                    path_i: self.path_i,
                    item_i_seq_end,
                })
            }
        }
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    serde::forward_to_deserialize_any! {
        str string bool
        i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char
        bytes byte_buf unit unit_struct newtype_struct tuple
        tuple_struct enum identifier ignored_any
    }
}

struct KeyDeserializer<'a> {
    key_part: KeyPart<'a>,
}

impl<'de> de::Deserializer<'de> for KeyDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        match self.key_part {
            KeyPart::Field(str) => visitor.visit_borrowed_str(str),
            _ => Err(Error::ExpectedKeyField),
        }
    }

    serde::forward_to_deserialize_any! {
        str string bool option map struct
        i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char
        bytes byte_buf unit unit_struct newtype_struct seq tuple
        tuple_struct enum identifier ignored_any
    }
}

struct SeqDeserializer<'de: 'b, 'b> {
    data: &'b [(&'de [u8], &'de [u8])],
    item_i: &'b mut usize,
    path_i: &'b mut [usize],

    item_i_seq_end: usize,
}

impl<'de: 'b, 'b> de::SeqAccess<'de> for SeqDeserializer<'de, 'b> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        debug_assert!(
            *self.item_i <= self.item_i_seq_end,
            "the seq should always end on the equality, so this will never happen"
        );

        // end of rows for the current map
        if *self.item_i == self.item_i_seq_end {
            return Ok(None);
        }

        let res = seed.deserialize(ValueDeserializer {
            data: self.data,
            item_i: self.item_i,
            path_i: self.path_i,
        });

        // seq ended
        if *self.item_i == self.item_i_seq_end {
            return res.map(Some);
        }

        // skip the row if we reach its end
        if self.path_i[*self.item_i] >= self.data[*self.item_i].0.len() {
            *self.item_i += 1;
        }

        res.map(Some)
    }
}

struct MapDeserializer<'de: 'b, 'b> {
    data: &'b [(&'de [u8], &'de [u8])],
    item_i: &'b mut usize,
    path_i: &'b mut [usize],

    item_i_map_end: usize,
}

impl<'de: 'b, 'b> de::MapAccess<'de> for MapDeserializer<'de, 'b> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        debug_assert!(
            *self.item_i <= self.item_i_map_end,
            "the map should always end on the equality, so this will never happen"
        );

        // end of rows for the current map
        if *self.item_i == self.item_i_map_end {
            return Ok(None);
        }

        // end of current row
        if self.path_i[*self.item_i] >= self.data[*self.item_i].0.len() {
            return Ok(None);
        }

        // decode and consume the key
        let res = seed.deserialize(KeyDeserializer {
            key_part: key::decode_part(self.data[*self.item_i].0, &mut self.path_i[*self.item_i])?,
        });

        res.map(Some)
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        let res = seed.deserialize(ValueDeserializer {
            data: self.data,
            item_i: self.item_i,
            path_i: self.path_i,
        });

        // map ended
        if *self.item_i == self.item_i_map_end {
            return res;
        }

        // skip the row if we reach its end
        if self.path_i[*self.item_i] >= self.data[*self.item_i].0.len() {
            *self.item_i += 1;
        }

        res
    }
}

/// A `(key, value)` row borrowed from a row blob.
pub type RowRef<'a> = (&'a [u8], &'a [u8]);

/// Splits a row blob in the layout
/// `[u32 row_count][[u16 key_len][u32 val_len] * row_count][[key][val] * row_count]`
/// into `(key, value)` slices, validating every length against the buffer.
pub fn split_rows(data: &[u8]) -> Result<Vec<RowRef<'_>>, Error> {
    let count_bytes = data.get(0..ROW_COUNT_BYTES).ok_or(Error::InvalidBytes)?;
    let num_rows = u32::from_le_bytes(count_bytes.try_into().unwrap()) as usize;

    let header_stride = KEY_LEN_BYTES + VAL_LEN_BYTES;
    let data_start = num_rows
        .checked_mul(header_stride)
        .and_then(|len| len.checked_add(ROW_COUNT_BYTES))
        .ok_or(Error::InvalidBytes)?;
    if data.len() < data_start {
        return Err(Error::InvalidBytes);
    }

    let mut rows = Vec::with_capacity(num_rows);
    let mut header_pos = ROW_COUNT_BYTES;
    let mut data_pos = data_start;
    for _ in 0..num_rows {
        let key_len = u16::from_le_bytes(
            data[header_pos..header_pos + KEY_LEN_BYTES]
                .try_into()
                .unwrap(),
        ) as usize;
        header_pos += KEY_LEN_BYTES;
        let val_len = u32::from_le_bytes(
            data[header_pos..header_pos + VAL_LEN_BYTES]
                .try_into()
                .unwrap(),
        ) as usize;
        header_pos += VAL_LEN_BYTES;

        let key_end = data_pos.checked_add(key_len).ok_or(Error::InvalidBytes)?;
        let val_end = key_end.checked_add(val_len).ok_or(Error::InvalidBytes)?;
        if val_end > data.len() {
            return Err(Error::InvalidBytes);
        }

        rows.push((&data[data_pos..key_end], &data[key_end..val_end]));
        data_pos = val_end;
    }

    Ok(rows)
}

/// Serializes `(key, value)` rows into the layout read by [`split_rows`].
pub fn join_rows<K: AsRef<[u8]>, V: AsRef<[u8]>>(rows: &[(K, V)]) -> Vec<u8> {
    let payload: usize = rows
        .iter()
        .map(|(k, v)| k.as_ref().len() + v.as_ref().len())
        .sum();
    let mut data = Vec::with_capacity(
        ROW_COUNT_BYTES + rows.len() * (KEY_LEN_BYTES + VAL_LEN_BYTES) + payload,
    );

    data.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (key, value) in rows {
        data.extend_from_slice(&(key.as_ref().len() as u16).to_le_bytes());
        data.extend_from_slice(&(value.as_ref().len() as u32).to_le_bytes());
    }
    for (key, value) in rows {
        data.extend_from_slice(key.as_ref());
        data.extend_from_slice(value.as_ref());
    }

    data
}

/// Deserializes a row blob (see [`split_rows`]) into `T`.
pub fn from_bytes<'de, T: serde::Deserialize<'de>>(data: &'de [u8]) -> Result<T, Error> {
    from_rows(split_rows(data)?.as_slice())
}

pub fn from_rows<'de, T: serde::Deserialize<'de>>(
    data: &[(&'de [u8], &'de [u8])],
) -> Result<T, Error> {
    let mut item_i: usize = 0;
    let mut path_i = vec![0; data.len()];

    T::deserialize(ValueDeserializer::<'de, '_> {
        data,
        item_i: &mut item_i,
        path_i: path_i.as_mut_slice(),
    })
}

struct MapKeySerializer {
    output: Option<String>,
}

impl ser::Serializer for &mut MapKeySerializer {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = ser::Impossible<(), Error>;
    type SerializeMap = ser::Impossible<(), Error>;
    type SerializeStruct = ser::Impossible<(), Error>;
    type SerializeTuple = ser::Impossible<(), Error>;
    type SerializeTupleStruct = ser::Impossible<(), Error>;
    type SerializeTupleVariant = ser::Impossible<(), Error>;
    type SerializeStructVariant = ser::Impossible<(), Error>;

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.output = Some(v.to_string());
        Ok(())
    }

    fn serialize_u128(self, _: u128) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_i128(self, _: i128) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        Err(Error::MapKeyNotString)
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        Err(Error::MapKeyNotString)
    }

    fn serialize_some<T>(self, _: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        Err(Error::MapKeyNotString)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_i8(self, _: i8) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_u8(self, _: u8) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_i16(self, _: i16) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_i32(self, _: i32) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_i64(self, _: i64) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_u16(self, _: u16) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_u32(self, _: u32) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_u64(self, _: u64) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_f32(self, _: f32) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_f64(self, _: f64) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_bool(self, _: bool) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_char(self, _: char) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_bytes(self, _: &[u8]) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(Error::MapKeyNotString)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(Error::MapKeyNotString)
    }
}

struct ValueSerializer<'ser> {
    data: &'ser mut Vec<SerializedRow>,
    current_path: SerializedRowKey,
}

impl<'ser> ser::Serializer for ValueSerializer<'ser> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = SeqSerializer<'ser>;
    type SerializeMap = MapSerializer<'ser>;
    type SerializeStruct = MapSerializer<'ser>;
    type SerializeTuple = ser::Impossible<(), Error>;
    type SerializeTupleStruct = ser::Impossible<(), Error>;
    type SerializeTupleVariant = ser::Impossible<(), Error>;
    type SerializeStructVariant = ser::Impossible<(), Error>;

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(1)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Boolean(v))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(2)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer((v as i64).into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(3)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer((v as i64).into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(5)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer((v as i64).into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(9)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer(v.into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(2)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer((v as i64).into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(3)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer((v as i64).into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(5)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer((v as i64).into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(9)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Integer(v.into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_i128(self, _: i128) -> Result<Self::Ok, Self::Error> {
        Err(Error::MessagePackNumber)
    }

    fn serialize_u128(self, _: u128) -> Result<Self::Ok, Self::Error> {
        Err(Error::MessagePackNumber)
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(5)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::F32(v))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(9)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::F64(v))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(2)));
        rmpv::encode::write_value_ref(
            &mut last.1.0,
            &rmpv::ValueRef::String(v.to_string().as_str().into()),
        )
        .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        let mut last = SerializedRow(
            self.current_path,
            SerializedRowValue(Vec::with_capacity(1 + v.len())),
        );
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::String(v.into()))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        let mut last = SerializedRow(
            self.current_path,
            SerializedRowValue(Vec::with_capacity(1 + v.len())),
        );
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Binary(v))
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(1)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Nil)
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        let mut last = SerializedRow(self.current_path, SerializedRowValue(Vec::with_capacity(1)));
        rmpv::encode::write_value_ref(&mut last.1.0, &rmpv::ValueRef::Nil)
            .map_err(|_| Error::EncodeValue)?;
        self.data.push(last);
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        Err(Error::UnsupportedValue)
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        value.serialize(self)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Ok(SeqSerializer {
            data: self.data,
            initial_path_size: self.current_path.0.len(),
            current_path: self.current_path,
            current_index: 0,
        })
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Error> {
        Err(Error::UnsupportedValue)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        Err(Error::UnsupportedValue)
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Ok(MapSerializer {
            data: self.data,
            initial_path_size: self.current_path.0.len(),
            current_path: self.current_path,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        self.serialize_map(Some(_len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error::MessagePackTuple)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<(), Error> {
        Err(Error::MessagePackTuple)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        Err(Error::MessagePackTuple)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error::MessagePackTuple)
    }
}

struct SeqSerializer<'ser> {
    data: &'ser mut Vec<SerializedRow>,
    current_path: SerializedRowKey,
    initial_path_size: usize,
    current_index: u32,
}

impl<'ser> ser::SerializeSeq for SeqSerializer<'ser> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        self.current_path.0.truncate(self.initial_path_size);

        let key_part = KeyPart::Index(self.current_index);
        self.current_index += 1;

        let required_size = key::measure_part(&key_part);
        let mut value_path = SerializedRowKey(vec![0u8; self.initial_path_size + required_size]);
        value_path.0[0..self.initial_path_size].copy_from_slice(&self.current_path.0);

        let mut i = self.initial_path_size;
        key::encode_part(&key_part, &mut value_path.0, &mut i);

        value.serialize(ValueSerializer {
            data: self.data,
            current_path: value_path,
        })?;

        Ok(())
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

struct MapSerializer<'ser> {
    data: &'ser mut Vec<SerializedRow>,
    initial_path_size: usize,
    current_path: SerializedRowKey,
}

impl<'ser> ser::SerializeStruct for MapSerializer<'ser> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        self.current_path.0.truncate(self.initial_path_size);

        let key_part = KeyPart::Field(key);

        let required_size = key::measure_part(&key_part);
        let mut value_path = SerializedRowKey(vec![0u8; self.initial_path_size + required_size]);
        value_path.0[0..self.initial_path_size].copy_from_slice(&self.current_path.0);

        let mut i = self.initial_path_size;
        key::encode_part(&key_part, &mut value_path.0, &mut i);

        value.serialize(ValueSerializer {
            data: self.data,
            current_path: value_path,
        })
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl<'ser> ser::SerializeMap for MapSerializer<'ser> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T>(&mut self, key: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        self.current_path.0.truncate(self.initial_path_size);

        let mut mks = MapKeySerializer { output: None };
        key.serialize(&mut mks)?;
        let key_part = KeyPart::Field(&mks.output.ok_or(Error::MapKeyNotString)?);

        let required_size = key::measure_part(&key_part);
        self.current_path
            .0
            .extend(std::iter::repeat_n(0, required_size));

        let mut i = self.initial_path_size;
        key::encode_part(&key_part, &mut self.current_path.0, &mut i);

        Ok(())
    }

    fn serialize_value<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + ser::Serialize,
    {
        value.serialize(ValueSerializer {
            data: self.data,
            current_path: self.current_path.clone(),
        })?;

        Ok(())
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

#[derive(PartialEq, PartialOrd, Clone, Eq, Ord)]
pub struct SerializedRow(pub SerializedRowKey, pub SerializedRowValue);

#[derive(PartialEq, PartialOrd, Clone, Eq, Ord)]
pub struct SerializedRowKey(pub Vec<u8>);

#[derive(PartialEq, PartialOrd, Clone, Eq, Ord)]
pub struct SerializedRowValue(pub Vec<u8>);

impl std::fmt::Debug for SerializedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SerializedRow<")?;
        self.0.fmt(f)?;
        write!(f, " => ")?;
        self.1.fmt(f)?;
        write!(f, ">")?;
        Ok(())
    }
}

impl std::fmt::Debug for SerializedRowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = key::decode(&self.0).map_err(|_| std::fmt::Error)?;
        write!(f, "[")?;
        for (i, part) in key.iter().enumerate() {
            if i != 0 {
                write!(f, ", ")?;
            }

            match part {
                KeyPart::Field(str) => write!(f, "\"{}\"", str)?,
                KeyPart::Index(num) => write!(f, "{}", num)?,
            };
        }
        write!(f, "]")?;
        Ok(())
    }
}

impl std::fmt::Debug for SerializedRowValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return write!(f, "(empty)");
        }
        let mut read = std::io::Cursor::new(self.0.clone());
        let val = rmpv::decode::read_value(&mut read).map_err(|_| std::fmt::Error)?;
        val.fmt(f)
    }
}

pub fn to_rows<T: ser::Serialize>(value: T, dest: &mut Vec<SerializedRow>) -> Result<(), Error> {
    value.serialize(ValueSerializer {
        data: dest,
        current_path: SerializedRowKey(Vec::with_capacity(36)),
    })?;

    dest.sort_unstable();

    Ok(())
}

#[cfg(test)]
mod tests {
    use maplit::hashmap;
    use std::collections::HashMap;

    use super::*;

    fn encode_value(value: rmpv::ValueRef<'_>) -> Vec<u8> {
        let mut res = Vec::with_capacity(128);
        rmpv::encode::write_value_ref(&mut res, &value).unwrap();
        res
    }

    #[test]
    fn test_empty() {
        {
            let res = from_rows::<HashMap<String, String>>(&[]);
            assert!(matches!(res, Err(Error::EndOfRows)));
        }
        {
            let res = from_rows::<Vec<String>>(&[]);
            assert!(matches!(res, Err(Error::EndOfRows)));
        }
        {
            let res = from_rows::<u32>(&[]);
            assert!(matches!(res, Err(Error::EndOfRows)));
        }
    }

    #[test]
    fn test_vec_string() {
        let initial = vec![
            (
                key::to_bytes(&[KeyPart::Index(0)]),
                encode_value(rmpv::ValueRef::String("hello".into())),
            ),
            (
                key::to_bytes(&[KeyPart::Index(1)]),
                encode_value(rmpv::ValueRef::String("world".into())),
            ),
            (
                key::to_bytes(&[KeyPart::Index(2)]),
                encode_value(rmpv::ValueRef::String("test".into())),
            ),
        ];

        let source: Vec<(&[u8], &[u8])> = initial
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        let vec: Vec<String> = from_rows(&source).unwrap();
        assert_eq!(
            vec,
            vec!["hello".to_string(), "world".to_string(), "test".to_string()]
        );

        let mut serialized = Vec::new();
        to_rows(vec, &mut serialized).unwrap();
        assert_eq!(
            initial
                .into_iter()
                .map(|(k, v)| SerializedRow(SerializedRowKey(k), SerializedRowValue(v)))
                .collect::<Vec<SerializedRow>>(),
            serialized
        );
    }

    #[test]
    fn test_simple_map() {
        let initial = vec![
            (
                key::to_bytes(key_path!("a")),
                encode_value(rmpv::ValueRef::Integer(1.into())),
            ),
            (
                key::to_bytes(key_path!("b")),
                encode_value(rmpv::ValueRef::Integer(2.into())),
            ),
        ];

        let source: Vec<(&[u8], &[u8])> = initial
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        let map: HashMap<String, u32> = from_rows(&source).unwrap();
        assert_eq!(map, hashmap! {"a".to_string() => 1, "b".to_string() => 2});

        let mut serialized = Vec::new();
        to_rows(map, &mut serialized).unwrap();
        assert_eq!(
            initial
                .into_iter()
                .map(|(k, v)| SerializedRow(SerializedRowKey(k), SerializedRowValue(v)))
                .collect::<Vec<SerializedRow>>(),
            serialized
        );
    }

    #[test]
    fn test_slightly_more_complicated_map() {
        #[derive(Debug, serde::Deserialize, serde::Serialize)]
        struct Thing {
            #[serde(rename = "o")]
            other: bool,

            #[serde(rename = "a")]
            map: HashMap<String, u32>,
        }

        let initial = vec![
            (
                key::to_bytes(key_path!("a", "f")),
                encode_value(rmpv::ValueRef::Integer(1.into())),
            ),
            (
                key::to_bytes(key_path!("a", "g")),
                encode_value(rmpv::ValueRef::Integer(2.into())),
            ),
            (
                key::to_bytes(key_path!("a", "h")),
                encode_value(rmpv::ValueRef::Integer(3.into())),
            ),
            (
                key::to_bytes(key_path!("o")),
                encode_value(rmpv::ValueRef::Boolean(true)),
            ),
        ];

        let source: Vec<(&[u8], &[u8])> = initial
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        let thing: Thing = from_rows(&source).unwrap();

        assert!(thing.other);
        assert_eq!(
            thing.map,
            hashmap! {"f".to_string() => 1, "g".to_string() => 2, "h".to_string() => 3}
        );

        let mut serialized = Vec::new();
        to_rows(thing, &mut serialized).unwrap();
        assert_eq!(
            initial
                .into_iter()
                .map(|(k, v)| SerializedRow(SerializedRowKey(k), SerializedRowValue(v)))
                .collect::<Vec<SerializedRow>>(),
            serialized
        );
    }

    #[test]
    fn test_basic() {
        #[derive(Debug, serde::Deserialize, serde::Serialize)]
        struct User {
            name: String,
            age: u8,

            #[serde(skip_serializing_if = "Option::is_none")]
            hair: Option<String>,

            #[serde(default)]
            kids: Vec<User>,
        }

        let mut initial = vec![
            (
                key::to_bytes(&[KeyPart::Field("b"), KeyPart::Field("age")]),
                encode_value(rmpv::ValueRef::Integer(32.into())),
            ),
            (
                key::to_bytes(&[KeyPart::Field("b"), KeyPart::Field("hair")]),
                encode_value(rmpv::ValueRef::String("black".into())),
            ),
            (
                key::to_bytes(&[KeyPart::Field("b"), KeyPart::Field("name")]),
                encode_value(rmpv::ValueRef::String("bob".into())),
            ),
            (
                key::to_bytes(&[KeyPart::Field("c"), KeyPart::Field("age")]),
                encode_value(rmpv::ValueRef::Integer(37.into())),
            ),
            (
                key::to_bytes(&[KeyPart::Field("c"), KeyPart::Field("name")]),
                encode_value(rmpv::ValueRef::String("carol".into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("c"),
                    KeyPart::Field("kids"),
                    KeyPart::Index(0),
                    KeyPart::Field("name"),
                ]),
                encode_value(rmpv::ValueRef::String("ann".into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("c"),
                    KeyPart::Field("kids"),
                    KeyPart::Index(0),
                    KeyPart::Field("age"),
                ]),
                encode_value(rmpv::ValueRef::Integer(8.into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("c"),
                    KeyPart::Field("kids"),
                    KeyPart::Index(1),
                    KeyPart::Field("name"),
                ]),
                encode_value(rmpv::ValueRef::String("chris".into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("c"),
                    KeyPart::Field("kids"),
                    KeyPart::Index(1),
                    KeyPart::Field("age"),
                ]),
                encode_value(rmpv::ValueRef::Integer(4.into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("c"),
                    KeyPart::Field("kids"),
                    KeyPart::Index(2),
                    KeyPart::Field("name"),
                ]),
                encode_value(rmpv::ValueRef::String("coco".into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("c"),
                    KeyPart::Field("kids"),
                    KeyPart::Index(2),
                    KeyPart::Field("age"),
                ]),
                encode_value(rmpv::ValueRef::Integer(1.into())),
            ),
            (
                key::to_bytes(&[KeyPart::Field("c"), KeyPart::Field("hair")]),
                encode_value(rmpv::ValueRef::String("white".into())),
            ),
        ];

        let source: Vec<(&[u8], &[u8])> = initial
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        let users: HashMap<String, User> = from_rows(&source).unwrap();
        assert_eq!(users.get("b").unwrap().hair, Some("black".to_string()));
        assert_eq!(users.get("c").unwrap().kids[2].name, "coco");
        assert_eq!(users.get("c").unwrap().kids[0].age, 8);

        let mut serialized = Vec::new();
        to_rows(users, &mut serialized).unwrap();
        initial.sort_unstable();
        assert_eq!(
            initial
                .into_iter()
                .map(|(k, v)| SerializedRow(SerializedRowKey(k), SerializedRowValue(v)))
                .collect::<Vec<SerializedRow>>(),
            serialized
        );
    }

    #[test]
    fn test_struct_with_vec_map() {
        #[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
        struct Thing {
            first: bool,
            second: f64,
            third: (),
            fourth: HashMap<String, String>,
        }

        #[derive(Debug, serde::Deserialize, serde::Serialize)]
        struct TestStruct {
            vec_field: Vec<i64>,
            map_field: HashMap<String, Thing>,
            vec_map_field: Vec<HashMap<String, i32>>,
        }

        let mut initial = vec![
            // vec_field
            (
                key::to_bytes(&[KeyPart::Field("vec_field"), KeyPart::Index(0)]),
                encode_value(rmpv::ValueRef::Integer(1.into())),
            ),
            (
                key::to_bytes(&[KeyPart::Field("vec_field"), KeyPart::Index(1)]),
                encode_value(rmpv::ValueRef::Integer(2.into())),
            ),
            // map_field - key1
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key1"),
                    KeyPart::Field("first"),
                ]),
                encode_value(rmpv::ValueRef::Boolean(true)),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key1"),
                    KeyPart::Field("second"),
                ]),
                encode_value(rmpv::ValueRef::F64(1.5)),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key1"),
                    KeyPart::Field("third"),
                ]),
                encode_value(rmpv::ValueRef::Nil),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key1"),
                    KeyPart::Field("fourth"),
                    KeyPart::Field("inner1"),
                ]),
                encode_value(rmpv::ValueRef::String("val1".into())),
            ),
            // map_field - key2
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key2"),
                    KeyPart::Field("first"),
                ]),
                encode_value(rmpv::ValueRef::Boolean(false)),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key2"),
                    KeyPart::Field("second"),
                ]),
                encode_value(rmpv::ValueRef::F64(2.5)),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key2"),
                    KeyPart::Field("third"),
                ]),
                encode_value(rmpv::ValueRef::Nil),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key2"),
                    KeyPart::Field("fourth"),
                    KeyPart::Field("inner2"),
                ]),
                encode_value(rmpv::ValueRef::String("val2".into())),
            ),
            // map_field - key3
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key3"),
                    KeyPart::Field("first"),
                ]),
                encode_value(rmpv::ValueRef::Boolean(true)),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key3"),
                    KeyPart::Field("second"),
                ]),
                encode_value(rmpv::ValueRef::F64(3.5)),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key3"),
                    KeyPart::Field("third"),
                ]),
                encode_value(rmpv::ValueRef::Nil),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("map_field"),
                    KeyPart::Field("key3"),
                    KeyPart::Field("fourth"),
                    KeyPart::Field("inner3"),
                ]),
                encode_value(rmpv::ValueRef::String("val3".into())),
            ),
            // vec_map_field
            (
                key::to_bytes(&[
                    KeyPart::Field("vec_map_field"),
                    KeyPart::Index(0),
                    KeyPart::Field("inner1"),
                ]),
                encode_value(rmpv::ValueRef::Integer(10.into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("vec_map_field"),
                    KeyPart::Index(0),
                    KeyPart::Field("inner2"),
                ]),
                encode_value(rmpv::ValueRef::Integer(20.into())),
            ),
            (
                key::to_bytes(&[
                    KeyPart::Field("vec_map_field"),
                    KeyPart::Index(1),
                    KeyPart::Field("inner3"),
                ]),
                encode_value(rmpv::ValueRef::Integer(30.into())),
            ),
        ];

        let source: Vec<(&[u8], &[u8])> = initial
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        let result: TestStruct = from_rows(&source).unwrap();

        assert_eq!(result.vec_field, vec![1, 2]);

        // check map_field with Thing structs
        let mut fourth1 = HashMap::new();
        fourth1.insert("inner1".to_string(), "val1".to_string());
        let thing1 = Thing {
            first: true,
            second: 1.5,
            third: (),
            fourth: fourth1,
        };

        let mut fourth2 = HashMap::new();
        fourth2.insert("inner2".to_string(), "val2".to_string());
        let thing2 = Thing {
            first: false,
            second: 2.5,
            third: (),
            fourth: fourth2,
        };

        let mut fourth3 = HashMap::new();
        fourth3.insert("inner3".to_string(), "val3".to_string());
        let thing3 = Thing {
            first: true,
            second: 3.5,
            third: (),
            fourth: fourth3,
        };

        assert_eq!(result.map_field.get("key1").unwrap(), &thing1);
        assert_eq!(result.map_field.get("key2").unwrap(), &thing2);
        assert_eq!(result.map_field.get("key3").unwrap(), &thing3);

        // check vec_map_field
        assert_eq!(
            result.vec_map_field,
            vec![
                hashmap! {
                        "inner1".to_string() => 10,
                        "inner2".to_string() => 20,
                },
                hashmap! {
                    "inner3".to_string() => 30
                }
            ]
        );

        let mut serialized = Vec::new();
        to_rows(result, &mut serialized).unwrap();
        initial.sort_unstable();
        assert_eq!(
            initial
                .into_iter()
                .map(|(k, v)| SerializedRow(SerializedRowKey(k), SerializedRowValue(v)))
                .collect::<Vec<SerializedRow>>(),
            serialized
        );
    }

    #[test]
    fn test_split_join_rows() {
        let rows = vec![(vec![1u8, 2], vec![3u8]), (vec![], vec![4u8, 5, 6])];
        let blob = join_rows(&rows);
        let split = split_rows(&blob).unwrap();
        assert_eq!(
            split,
            vec![(&[1u8, 2][..], &[3u8][..]), (&[][..], &[4u8, 5, 6][..])]
        );

        // every truncation must be rejected rather than panic
        for len in 0..blob.len() {
            assert!(split_rows(&blob[..len]).is_err());
        }

        // absurd row counts must not overflow
        assert!(split_rows(&u32::MAX.to_le_bytes()).is_err());
    }
}
