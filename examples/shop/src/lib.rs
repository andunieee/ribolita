use anyhow::anyhow;
use ribolita_common::{entry::Entry, key::KeyPart, key_path, value};
use rmpv::Value;

use ribolita_wasm_shared::{
    StateError, logger::Logger, return_err, state_get, state_push, state_set,
};

static LOGGER: Logger = Logger;

#[unsafe(no_mangle)]
pub extern "C" fn init() {
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
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

#[derive(Debug, serde::Deserialize, serde::Serialize, Default)]
#[allow(dead_code)]
struct State {
    sales: Vec<Sale>,
    items: Vec<Item>,
    next_item: u32,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Sale {
    date: u64,
    entries: Vec<SaleEntry>,
    revenue: u32,
    discount: u32,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct SaleEntry {
    item_ref: u32,
    quantity: u32,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Item {
    name: String,
    price: u32,
}

fn update(entry: &Entry) -> Result<(), anyhow::Error> {
    match entry.kind.as_str() {
        "item" => {
            let mut price: Option<u32> = None;
            let mut name: Option<String> = None;

            for (tag, value) in &entry.values {
                match tag.as_str() {
                    "price" => match value {
                        Value::Integer(p) => {
                            // grab price
                            price = Some(p.as_u64().ok_or_else(|| anyhow!("invalid price"))? as u32)
                        }
                        _ => anyhow::bail!("invalid price type"),
                    },
                    "name" => match value {
                        Value::String(s) => {
                            // grab name
                            name = s.as_str().map(str::to_string);
                        }
                        _ => anyhow::bail!("invalid name type"),
                    },
                    other => anyhow::bail!("unexpected item field '{other}'"),
                }
            }

            if let (Some(price), Some(name)) = (price, name) {
                let next_item = match state_get::<u32>(key_path!("next_item")) {
                    Ok(v) => v,
                    Err(StateError::StateDeserialize(value::Error::EndOfRows)) => 0,
                    Err(err) => {
                        return Err(err)?;
                    }
                };
                state_set(key_path!("items", next_item), Item { price, name })?;
                state_set(key_path!("next_item"), next_item + 1)?;
            } else if price.is_none() {
                anyhow::bail!("invalid item registration: missing price");
            } else {
                anyhow::bail!("invalid item registration: missing name");
            }
        }
        "sale" => {
            let mut sale = Sale {
                date: entry.when,
                entries: Vec::with_capacity(entry.values.len()),
                revenue: 0,
                discount: 0,
            };
            let mut actual_paid: Option<u32> = None;

            for (tag, value) in &entry.values {
                match tag.as_str() {
                    "item" => match value {
                        Value::Map(map) => {
                            let mut sale_entry = SaleEntry {
                                item_ref: 0,
                                quantity: 1,
                            };

                            for (k, v) in map {
                                match k.as_str() {
                                    Some("id") => {
                                        sale_entry.item_ref = v
                                            .as_u64()
                                            .ok_or_else(|| anyhow!("id must be integer"))?
                                            as u32;
                                    }
                                    Some("quantity") => {
                                        sale_entry.quantity = v
                                            .as_u64()
                                            .ok_or_else(|| anyhow!("quantity must be integer"))?
                                            as u32
                                    }
                                    _ => anyhow::bail!("unexpected field"),
                                }
                            }
                            if let Ok(item) =
                                state_get::<Item>(key_path!("items", sale_entry.item_ref))
                            {
                                sale.revenue = item
                                    .price
                                    .checked_mul(sale_entry.quantity)
                                    .and_then(|total| sale.revenue.checked_add(total))
                                    .ok_or_else(|| anyhow!("sale total overflows"))?;
                            } else {
                                anyhow::bail!("item not found");
                            }

                            sale.entries.push(sale_entry);
                        }
                        _ => anyhow::bail!("item must be mapping"),
                    },
                    "actual_paid" => {
                        actual_paid = Some(
                            value
                                .as_u64()
                                .ok_or_else(|| anyhow!("actual_paid must be integer"))?
                                as u32,
                        );
                    }
                    _ => anyhow::bail!("unexpected value"),
                }
            }

            if let Some(actual) = actual_paid {
                sale.discount = sale
                    .revenue
                    .checked_sub(actual)
                    .ok_or_else(|| anyhow!("paid more than the total amount"))?;
                sale.revenue = actual;
                state_push(key_path!("sales"), sale)?;
            } else {
                anyhow::bail!("invalid sale declaration");
            }
        }
        _ => {}
    };

    Ok(())
}
