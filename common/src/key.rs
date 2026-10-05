#[derive(Debug, PartialEq, Eq, Clone)]
pub enum KeyPart<'s> {
    Field(&'s str),
    Index(u32),
}

impl std::fmt::Display for KeyPart<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyPart::Field(str) => write!(f, "KeyPart:Field(\"{}\")", str),
            KeyPart::Index(num) => write!(f, "KeyPart::Index({})", num),
        }
    }
}

#[macro_export]
macro_rules! key_path {
    ($($item:expr),* $(,)?) => {
        &[$(KeyPart::from($item)),*] as &[KeyPart]
    };
}

/// Parses a dotted path like `"items.3.name"`. An empty string is the root path.
pub fn parse<'s>(s: &'s str) -> Result<Vec<KeyPart<'s>>, FieldTooLong> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    s.split('.')
        .map(|part| {
            if let Ok(num) = part.parse::<u32>() {
                Ok(KeyPart::Index(num))
            } else if part.len() > MAX_FIELD_LEN {
                Err(FieldTooLong(part.to_string()))
            } else {
                Ok(KeyPart::Field(part))
            }
        })
        .collect()
}

/// Field names are length-prefixed with 7 bits.
pub const MAX_FIELD_LEN: usize = 127;

#[derive(thiserror::Error, Debug)]
#[error("key field '{0}' is longer than {MAX_FIELD_LEN} bytes")]
pub struct FieldTooLong(pub String);

const FIELD: u8 = 1;
const INDEX: u8 = 0;

pub fn measure(key: &[KeyPart]) -> usize {
    let mut size = 0;
    for part in key {
        size += measure_part(part);
    }
    size
}

#[inline]
pub fn measure_part(part: &KeyPart) -> usize {
    match part {
        KeyPart::Field(str) => {
            let size = 1 + str.len();
            if str.len() > MAX_FIELD_LEN {
                panic!("string key can't be bigger than {MAX_FIELD_LEN}");
            }
            size
        }
        KeyPart::Index(num) => {
            if *num < 126 {
                1
            } else if *num < 65535 {
                1 + 2
            } else {
                1 + 4
            }
        }
    }
}

pub fn to_bytes(key: &[KeyPart]) -> Vec<u8> {
    let size = measure(key);
    let mut buf = vec![0; size];
    encode(key, &mut buf);
    buf
}

pub fn encode(key: &[KeyPart], buf: &mut [u8]) {
    let mut i = 0;
    for part in key {
        encode_part(part, buf, &mut i);
    }
}

#[inline]
pub fn encode_part(key_part: &KeyPart, buf: &mut [u8], i: &mut usize) {
    match key_part {
        KeyPart::Field(str) => {
            buf[*i] = FIELD << 7 | str.len() as u8;
            *i += 1;
            buf[*i..*i + str.len()].copy_from_slice(str.as_bytes());
            *i += str.len();
        }
        KeyPart::Index(num) => {
            if *num < 126 {
                buf[*i] = INDEX << 7 | *num as u8;
                *i += 1;
            } else if *num < 65535 {
                buf[*i] = INDEX << 7 | 126;
                *i += 1;
                buf[*i..*i + 2].copy_from_slice(&(*num as u16).to_be_bytes());
                *i += 2;
            } else {
                buf[*i] = INDEX << 7 | 127;
                *i += 1;
                buf[*i..*i + 4].copy_from_slice(&num.to_be_bytes());
                *i += 4;
            }
        }
    }
}

pub fn decode<'k>(buf: &'k [u8]) -> Result<Vec<KeyPart<'k>>, DecodeError> {
    let mut i = 0;
    let mut key = Vec::with_capacity(6);

    while i < buf.len() {
        let part = decode_part(buf, &mut i)?;
        key.push(part);
    }

    Ok(key)
}

pub fn decode_part<'k>(buf: &'k [u8], i: &'_ mut usize) -> Result<KeyPart<'k>, DecodeError> {
    match (buf[*i] & 0b10000000) >> 7 {
        FIELD => {
            let size = (buf[*i] & 0b01111111) as usize;
            *i += 1;
            if buf.len() < *i + size {
                return Err(DecodeError::MissingBytes(*i, size));
            }
            let part = KeyPart::Field(
                core::str::from_utf8(&buf[*i..*i + size]).map_err(|_| DecodeError::InvalidUTF8)?,
            );
            *i += size;
            Ok(part)
        }
        INDEX => {
            let size = (buf[*i] & 0b01111111) as usize;
            let num = match size {
                126 => {
                    *i += 1;
                    if buf.len() < *i + 2 {
                        return Err(DecodeError::MissingBytes(*i, 2));
                    }
                    let num = u16::from_be_bytes(buf[*i..*i + 2].try_into().unwrap());
                    *i += 2;
                    u32::from(num)
                }
                127 => {
                    *i += 1;
                    if buf.len() < *i + 4 {
                        return Err(DecodeError::MissingBytes(*i, 4));
                    }
                    let num = u32::from_be_bytes(buf[*i..*i + 4].try_into().unwrap());
                    *i += 4;
                    num
                }
                _ => {
                    let num = buf[*i] as u32;
                    *i += 1;
                    num
                }
            };
            Ok(KeyPart::Index(num))
        }
        _ => panic!("never happens because binary"),
    }
}

#[derive(thiserror::Error, Debug, serde::Deserialize)]
pub enum DecodeError {
    #[error("missing bytes (reading {0} from {1})")]
    MissingBytes(usize, usize),

    #[error("invalid string field")]
    InvalidUTF8,
}

impl<'s> From<&'s str> for KeyPart<'s> {
    fn from(s: &'s str) -> Self {
        KeyPart::Field(s)
    }
}

impl From<usize> for KeyPart<'_> {
    fn from(n: usize) -> Self {
        KeyPart::Index(n as u32)
    }
}

impl From<i32> for KeyPart<'_> {
    fn from(n: i32) -> Self {
        KeyPart::Index(n as u32)
    }
}

impl From<i64> for KeyPart<'_> {
    fn from(n: i64) -> Self {
        KeyPart::Index(n as u32)
    }
}

impl From<u32> for KeyPart<'_> {
    fn from(n: u32) -> Self {
        KeyPart::Index(n)
    }
}

impl From<u64> for KeyPart<'_> {
    fn from(n: u64) -> Self {
        KeyPart::Index(n as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty() {
        let key = vec![];
        let mut buf = vec![0u8; measure(&key)];
        encode(&key, &mut buf);
        let empty: Vec<u8> = vec![];
        assert_eq!(buf, empty);
    }

    #[test]
    fn test_complex() {
        let key = vec![
            KeyPart::Field("test"),
            KeyPart::Field("test"),
            KeyPart::Index(129),
            KeyPart::Index(129),
            KeyPart::Field("another"),
            KeyPart::Index(65536),
            KeyPart::Field("foo"),
            KeyPart::Index(126),
            KeyPart::Field("bar"),
            KeyPart::Index(127),
            KeyPart::Field("nnnnn"),
            KeyPart::Index(70000),
        ];
        let mut buf = vec![0u8; measure(&key)];
        encode(&key, &mut buf);
        let decoded = decode(&buf).unwrap();
        assert_eq!(key, decoded);
        assert_eq!(
            key,
            key_path!(
                "test", "test", 129, 129, "another", 65536, "foo", 126, "bar", 127, "nnnnn", 70000
            )
        );
    }

    #[test]
    fn test_missing_bytes() {
        let buf = vec![132, 90, 90, 90]; // FIELD len 4, but only 3 bytes afterwards
        let key = decode(&buf);
        assert!(matches!(key, Err(DecodeError::MissingBytes(1, 4))));

        let buf = vec![126, 90]; // INDEX asks for 2 more bytes to build the number, but they are missing
        let key = decode(&buf);
        assert!(matches!(key, Err(DecodeError::MissingBytes(1, 2))));

        let buf = vec![127, 90, 90, 90]; // INDEX asks for 4 more bytes to build the number, but they are missing
        let key = decode(&buf);
        assert!(matches!(key, Err(DecodeError::MissingBytes(1, 4))));
    }

    #[test]
    fn test_invalid_utf8() {
        let buf = vec![129, 0xc0]; // FIELD len 1, invalid utf8
        let key = decode(&buf);
        assert!(matches!(key, Err(DecodeError::InvalidUTF8)));
    }

    #[test]
    fn test_parse() {
        assert_eq!(parse("").unwrap(), vec![]);
        assert!(parse(&"x".repeat(128)).is_err());

        let result = parse("field.123.another").unwrap();
        assert_eq!(
            result,
            vec![
                KeyPart::Field("field"),
                KeyPart::Index(123),
                KeyPart::Field("another")
            ]
        );

        let result2 = parse("0.hello.456").unwrap();
        assert_eq!(
            result2,
            vec![
                KeyPart::Index(0),
                KeyPart::Field("hello"),
                KeyPart::Index(456)
            ]
        );
    }
}
