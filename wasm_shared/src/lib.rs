#![allow(static_mut_refs)]
use std::alloc::{alloc, dealloc, Layout};

pub mod logger;

use ribolita_common::{
    key::{self, KeyPart},
    value::{self, to_rows, SerializedRow, SerializedRowKey, SerializedRowValue},
    Opcode, ROW_COUNT_BYTES,
};

// only exported on wasm: on native targets an unmangled `malloc` would replace libc's allocator
#[cfg_attr(target_arch = "wasm32", unsafe(no_mangle))]
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

pub fn state_get<T>(path: &[KeyPart]) -> Result<T, StateError>
where
    T: serde::de::DeserializeOwned,
{
    let mut key = vec![0u8; key::measure(path)];
    key::encode(path, &mut key);
    let ptr = unsafe { state_get_internal(key.as_ptr(), key.len()) };
    if (ptr as isize) <= 0 {
        return Err(StateError::Failed);
    }

    // the host allocated `[u32 blob_len][blob]` with our `malloc` and hands ownership to us
    unsafe {
        let len_bytes = core::slice::from_raw_parts(ptr, ROW_COUNT_BYTES);
        let blob_len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        let blob = core::slice::from_raw_parts(ptr.add(ROW_COUNT_BYTES), blob_len);
        let result = value::from_bytes::<T>(blob).map_err(StateError::from);
        let total = ROW_COUNT_BYTES + blob_len;
        dealloc(ptr as *mut u8, Layout::from_size_align_unchecked(total, 1));
        result
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
    let rows: Vec<(&[u8], &[u8])> = rows
        .iter()
        .map(|SerializedRow(SerializedRowKey(key), SerializedRowValue(val))| {
            (key.as_slice(), val.as_slice())
        })
        .collect();
    let buf = value::join_rows(&rows);

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
