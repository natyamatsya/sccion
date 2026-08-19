pub trait BincodeByteOrder {
    type Endian: crate::byteorder::ByteOrder + 'static;
}

/// Little-endian byte ordering.
#[derive(Copy, Clone)]
pub struct LittleEndian;

/// Big-endian byte ordering.
#[derive(Copy, Clone)]
pub struct BigEndian;

/// The native byte ordering of the current system.
#[derive(Copy, Clone)]
pub struct NativeEndian;

impl BincodeByteOrder for LittleEndian {
    type Endian = crate::byteorder::LittleEndian;
}

impl BincodeByteOrder for BigEndian {
    type Endian = crate::byteorder::BigEndian;
}

impl BincodeByteOrder for NativeEndian {
    type Endian = crate::byteorder::NativeEndian;
}
