use ribolita_common::{entry::Entry, key::KeyPart, key_path};
use ribolita_wasm_shared::{logger::Logger, return_err, state_decr, state_incr};

static LOGGER: Logger = Logger;

#[unsafe(no_mangle)]
pub extern "C" fn init() {
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
    log::warn!("...");
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `entry_ptr` must be valid for `entry_len` bytes and point to an encoded `Entry`.
pub unsafe extern "C" fn process(entry_ptr: *const u8, entry_len: usize) {
    let entry_slice = unsafe { core::slice::from_raw_parts(entry_ptr, entry_len) };
    let entry = rmp_serde::from_slice::<Entry>(entry_slice).expect("invalid entry");

    match update(&entry) {
        Ok(_) => {}
        Err(err) => {
            let err_str = err.to_string();
            unsafe { return_err(err_str.as_ptr(), err_str.len()) }
        }
    };
}

fn update(entry: &Entry) -> Result<(), anyhow::Error> {
    log::warn!("hello {}", entry);
    match entry.kind.as_str() {
        "increase" => state_incr(
            key_path!("counter"),
            entry.get_value::<i64>("amount").unwrap_or(1),
        )?,
        "decrease" => state_decr(
            key_path!("counter"),
            entry.get_value::<i64>("amount").unwrap_or(1),
        )?,
        _ => {}
    };

    Ok(())
}
