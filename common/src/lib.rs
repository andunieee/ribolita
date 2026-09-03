#[macro_use]
pub mod key;
pub mod entry;
pub mod rmp_json;
pub mod state;
pub mod value;

pub const ROW_COUNT_BYTES: usize = 4;
pub const KEY_LEN_BYTES: usize = 2;
pub const VAL_LEN_BYTES: usize = 4;

#[derive(Debug, num_enum::IntoPrimitive, num_enum::FromPrimitive)]
#[repr(i32)]
pub enum Opcode {
    Set = 1,
    Delete = 2,
    Push = 3,
    Pop = 4,
    Remove = 5,
    SwapRemove = 6,
    Incr = 7,
    Decr = 8,

    #[num_enum(default)]
    Unknown,
}
