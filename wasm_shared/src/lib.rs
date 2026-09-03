#![allow(static_mut_refs)]
use std::alloc::{alloc, Layout};

pub mod logger;

use ribolita_common::{
    key::{self, KeyPart},
    value::{self, to_rows, SerializedRow, SerializedRowKey, SerializedRowValue},
    Opcode, KEY_LEN_BYTES, ROW_COUNT_BYTES, VAL_LEN_BYTES,
};

#[unsafe(no_mangle)]
pub extern "C" fn malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return std::ptr::dangling_mut();
    }

    if let Ok(layout) = Layout::from_size_align(size, 1) {
        unsafe {
            let ptr = alloc(layout);
            if !ptr.is_null() {
                return ptr;
            }
        }
    }

    panic!("failed to alloc");
}

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn print_from_buffer(ptr: *const u8, len: usize);
    pub fn return_err(ptr: *const u8, len: usize);
    pub fn state_get_internal(key_ptr: *const u8, key_len: usize) -> *const u8;
    pub fn state_modify_internal(
        key_ptr: *const u8,
        key_len: usize,
        value_ptr: *const u8,
        value_len: usize,
        opcode: i32,
    ) -> i32;
}

pub fn external_println(str: String) {
    unsafe {
        let ptr = str.as_bytes().as_ptr();
        print_from_buffer(ptr, str.len());
    }
}

pub fn state_get<'de, T>(path: &[KeyPart]) -> Result<T, StateError>
where
    T: serde::de::Deserialize<'de>,
{
    unsafe {
        let mut key = vec![0u8; key::measure(path)];
        key::encode(path, &mut key);
        let ptr = state_get_internal(key.as_ptr(), key.len());

        // as the response we get a pointer to the state rows
        let count = core::slice::from_raw_parts(ptr, ROW_COUNT_BYTES);
        let buf_size = u32::from_le_bytes(count[0..ROW_COUNT_BYTES].try_into().unwrap()) as usize;
        let data = &core::slice::from_raw_parts(ptr, ROW_COUNT_BYTES + buf_size)[ROW_COUNT_BYTES..];
        let num_rows = usize::from_le_bytes(data[0..ROW_COUNT_BYTES].try_into().unwrap());
        let v: T = value::from_bytes(num_rows, data)?;
        Ok(v)
    }
}

#[inline]
fn state_modify<T>(path: &[KeyPart], data: T, opcode: Opcode) -> Result<(), StateError>
where
    T: serde::ser::Serialize,
{
    // encode key
    let mut key = vec![0u8; key::measure(path)];
    key::encode(path, &mut key);

    // encode value (value gets encoded into rows of keys and values)
    let mut rows = Vec::with_capacity(1024);
    to_rows(data, &mut rows)?;

    // now we compress all that into a blob of [n_rows][[key_len][val_len] * n_rows][[key][val] * n_rows]
    let mut buf = Vec::with_capacity(
        // estimate capacity needed based on first row
        ROW_COUNT_BYTES
            + rows.len()
                * rows
                    .first()
                    .map(
                        |SerializedRow(SerializedRowKey(key), SerializedRowValue(val))| {
                            KEY_LEN_BYTES + VAL_LEN_BYTES + key.len() + val.len()
                        },
                    )
                    .map(|est| est * 13 / 10 /* shoot up a little */)
                    .unwrap_or(0),
    );

    buf.extend_from_slice(&(rows.len() as u32).to_le_bytes()); // ROW_COUNT_BYTES
    rows.iter().for_each(
        |SerializedRow(SerializedRowKey(key), SerializedRowValue(val))| {
            buf.extend_from_slice(&(key.len() as u16).to_le_bytes()); // KEY_LEN_BYTES
            buf.extend_from_slice(&(val.len() as u32).to_le_bytes()); // VAL_LEN_BYTES
        },
    );
    rows.iter().for_each(
        |SerializedRow(SerializedRowKey(key), SerializedRowValue(val))| {
            buf.extend_from_slice(key);
            buf.extend_from_slice(val);
        },
    );

    unsafe {
        let res = state_modify_internal(
            key.as_ptr(),
            key.len(),
            buf.as_ptr(),
            buf.len(),
            opcode.into(),
        );
        if res < 0 {
            return Err(StateError::Failed);
        }
    }

    Ok(())
}

macro_rules! declare_state_modifier_fn {
    ($opname:ident, $opcode:expr) => {
        pub fn $opname<T>(path: &[KeyPart], data: T) -> Result<(), StateError>
        where
            T: serde::ser::Serialize,
        {
            state_modify(path, data, $opcode)
        }
    };
}

declare_state_modifier_fn!(state_set, Opcode::Set);
declare_state_modifier_fn!(state_delete, Opcode::Delete);
declare_state_modifier_fn!(state_push, Opcode::Push);
declare_state_modifier_fn!(state_pop, Opcode::Pop);
declare_state_modifier_fn!(state_remove, Opcode::Remove);
declare_state_modifier_fn!(state_incr, Opcode::Incr);
declare_state_modifier_fn!(state_decr, Opcode::Decr);

#[derive(Debug, thiserror::Error, serde::Deserialize)]
pub enum StateError {
    #[error("invalid data from host")]
    InvalidDataFromHost,

    #[error("state deserialize error: {0}")]
    StateDeserialize(#[from] value::Error),

    #[error("failed to decode key: {0}")]
    KeyDecode(#[from] key::DecodeError),

    #[error("failed")]
    Failed,
}
