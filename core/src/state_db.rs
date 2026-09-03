use ribolita_common::{KEY_LEN_BYTES, ROW_COUNT_BYTES, VAL_LEN_BYTES};
use std::collections::VecDeque;

use crate::database::{ReadTransaction, WriteTransaction};

pub trait StateDatabase {
    type ReadTxn<'db>: ReadTransaction
    where
        Self: 'db;
    type WriteTxn<'db>: WriteTransaction
    where
        Self: 'db;

    fn init(path: &std::path::Path) -> Result<Self, String>
    where
        Self: Sized;

    fn start_read_txn<'db>(&'db self) -> anyhow::Result<Self::ReadTxn<'db>>;
    fn start_write_txn<'db>(&'db self) -> anyhow::Result<Self::WriteTxn<'db>>;
}

/// helper function to deserialize rows from data format
/// this is shared between implementations
pub fn deserialize_rows(data: &[u8]) -> VecDeque<(&[u8], &[u8])> {
    let row_count = u32::from_le_bytes(data[0..ROW_COUNT_BYTES].try_into().unwrap()) as usize;
    let mut new_rows = VecDeque::with_capacity(row_count);

    // where the actual keys and values start (we'll move this as we read)
    let mut data_pos = ROW_COUNT_BYTES + (KEY_LEN_BYTES + VAL_LEN_BYTES) * row_count;

    for i in 0..row_count {
        let rel_key_len = u16::from_le_bytes(
            data[ROW_COUNT_BYTES + (KEY_LEN_BYTES + VAL_LEN_BYTES) * i
                ..ROW_COUNT_BYTES + (KEY_LEN_BYTES + VAL_LEN_BYTES) * i + KEY_LEN_BYTES]
                .try_into()
                .unwrap(),
        ) as usize;
        let val_len = u32::from_le_bytes(
            data[ROW_COUNT_BYTES + (KEY_LEN_BYTES + VAL_LEN_BYTES) * i + KEY_LEN_BYTES
                ..ROW_COUNT_BYTES
                    + (KEY_LEN_BYTES + VAL_LEN_BYTES) * i
                    + KEY_LEN_BYTES
                    + VAL_LEN_BYTES]
                .try_into()
                .unwrap(),
        ) as usize;

        let rel_key = &data[data_pos..data_pos + rel_key_len];
        let value = &data[data_pos + rel_key_len..data_pos + rel_key_len + val_len];
        data_pos += rel_key_len + val_len;

        new_rows.push_back((rel_key, value));
    }

    new_rows
}

/// helper function to serialize rows to bytes.
/// this is shared between implementations
pub fn serialize_rows(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();

    // write row count
    data.extend_from_slice(&(rows.len() as u32).to_le_bytes());

    // write key/value length headers
    for (key, value) in rows {
        data.extend_from_slice(&(key.len() as u16).to_le_bytes());
        data.extend_from_slice(&(value.len() as u32).to_le_bytes());
    }

    // write actual key/value data
    for (key, value) in rows {
        data.extend_from_slice(key);
        data.extend_from_slice(value);
    }

    data
}
