//! Bounds-checked helpers for fixed-width little-endian fields.

use crate::PackError;

/// Reads `N` bytes at `offset` without panicking on short input.
pub fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], PackError> {
    let end = offset.checked_add(N).ok_or(PackError::Overflow)?;
    let slice = bytes.get(offset..end).ok_or(PackError::Truncated {
        needed: end,
        available: bytes.len(),
    })?;
    let mut value = [0_u8; N];
    value.copy_from_slice(slice);
    Ok(value)
}

/// Reads a little-endian `u16`.
pub fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PackError> {
    read_array::<2>(bytes, offset).map(u16::from_le_bytes)
}

/// Reads a little-endian `u32`.
pub fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PackError> {
    read_array::<4>(bytes, offset).map(u32::from_le_bytes)
}

/// Reads a little-endian `u64`.
pub fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, PackError> {
    read_array::<8>(bytes, offset).map(u64::from_le_bytes)
}
