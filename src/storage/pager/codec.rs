//! Little-endian encoding and decoding helpers for on-disk integers.

use crate::storage::pager::error::PageError;

/// Writes a fixed-size `[u8; N]` array at byte offset `pos`, bounds-checked.
fn write_array<const N: usize>(
    buffer: &mut [u8],
    pos: usize,
    bytes: [u8; N],
) -> Result<(), PageError> {
    write_bytes(buffer, pos, &bytes)
}

/// Reads a fixed-size `[u8; N]` array at byte offset `pos`, bounds-checked.
fn read_array<const N: usize>(buffer: &[u8], pos: usize) -> Result<[u8; N], PageError> {
    <[u8; N]>::try_from(slice(buffer, pos, N)?).map_err(|_| PageError::OutOfBounds {
        len: N,
        offset: pos,
        buf_len: buffer.len(),
    })
}

/// Writes `value` at byte offset `pos` in little-endian. On error the buffer is not modified.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn write_u8(buffer: &mut [u8], pos: usize, value: u8) -> Result<(), PageError> {
    write_array(buffer, pos, value.to_le_bytes())
}

/// Writes `value` at byte offset `pos` in little-endian. On error the buffer is not modified.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn write_u16(buffer: &mut [u8], pos: usize, value: u16) -> Result<(), PageError> {
    write_array(buffer, pos, value.to_le_bytes())
}

/// Writes `value` at byte offset `pos` in little-endian. On error the buffer is not modified.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn write_u32(buffer: &mut [u8], pos: usize, value: u32) -> Result<(), PageError> {
    write_array(buffer, pos, value.to_le_bytes())
}

/// Writes `value` at byte offset `pos` in little-endian. On error the buffer is not modified.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn write_u64(buffer: &mut [u8], pos: usize, value: u64) -> Result<(), PageError> {
    write_array(buffer, pos, value.to_le_bytes())
}

/// Reads a little-endian `u8` at byte offset `pos`.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn read_u8(buffer: &[u8], pos: usize) -> Result<u8, PageError> {
    Ok(u8::from_le_bytes(read_array(buffer, pos)?))
}

/// Reads a little-endian `u16` at byte offset `pos`.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn read_u16(buffer: &[u8], pos: usize) -> Result<u16, PageError> {
    Ok(u16::from_le_bytes(read_array(buffer, pos)?))
}

/// Reads a little-endian `u32` at byte offset `pos`.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn read_u32(buffer: &[u8], pos: usize) -> Result<u32, PageError> {
    Ok(u32::from_le_bytes(read_array(buffer, pos)?))
}

/// Reads a little-endian `u64` at byte offset `pos`.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn read_u64(buffer: &[u8], pos: usize) -> Result<u64, PageError> {
    Ok(u64::from_le_bytes(read_array(buffer, pos)?))
}

/// Copies all of `bytes` into `buffer` starting at byte offset `pos`. On error the buffer is not modified.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn write_bytes(buffer: &mut [u8], pos: usize, bytes: &[u8]) -> Result<(), PageError> {
    let buffer_len = buffer.len();

    let end = pos.checked_add(bytes.len()).ok_or(PageError::OutOfBounds {
        offset: pos,
        len: bytes.len(),
        buf_len: buffer_len,
    })?;
    buffer
        .get_mut(pos..end)
        .ok_or(PageError::OutOfBounds {
            offset: pos,
            len: bytes.len(),
            buf_len: buffer_len,
        })?
        .copy_from_slice(bytes);

    Ok(())
}

/// Returns a borrowed view (no copy) of `n` bytes starting at byte offset `pos`.
///
/// # Errors
/// Will return Err if the position is out of bound of the buffer
pub fn slice(buffer: &[u8], pos: usize, n: usize) -> Result<&[u8], PageError> {
    let buffer_len = buffer.len();

    let end = pos.checked_add(n).ok_or(PageError::OutOfBounds {
        offset: pos,
        len: n,
        buf_len: buffer_len,
    })?;
    buffer.get(pos..end).ok_or(PageError::OutOfBounds {
        offset: pos,
        len: n,
        buf_len: buffer_len,
    })
}
