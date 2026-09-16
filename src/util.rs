//! Small helpers shared by the platform backends.

/// Copy `height` rows of `row_bytes` out of a buffer whose rows are `pitch` bytes apart.
///
/// Every backend hands its frames over as tightly packed `width * height * 4`, but the
/// producers pad rows to whatever alignment the hardware wants, so the padding has to come
/// off somewhere. Returns `None` when `pitch` is too small to hold a row, which is the one
/// shape that cannot be unpadded.
///
/// # Safety
///
/// `base` must point at `pitch * height` readable bytes.
pub(crate) unsafe fn pack_rows(
    base: *const u8,
    pitch: usize,
    row_bytes: usize,
    height: usize,
) -> Option<Vec<u8>> {
    if base.is_null() || pitch < row_bytes {
        return None;
    }
    unsafe {
        if pitch == row_bytes {
            return Some(core::slice::from_raw_parts(base, row_bytes * height).to_vec());
        }
        let padded = core::slice::from_raw_parts(base, pitch * height);
        let mut packed = Vec::with_capacity(row_bytes * height);
        for row in 0..height {
            let start = row * pitch;
            packed.extend_from_slice(&padded[start..start + row_bytes]);
        }
        Some(packed)
    }
}
