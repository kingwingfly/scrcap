//! Small helpers shared by the platform backends.

/// Byte span from the first pixel to the end of the last row, excluding trailing padding.
pub(crate) fn row_span(pitch: usize, row_bytes: usize, height: usize) -> Option<usize> {
    if row_bytes == 0 || pitch < row_bytes {
        return None;
    }
    let span = height
        .checked_sub(1)?
        .checked_mul(pitch)?
        .checked_add(row_bytes)?;
    (span <= isize::MAX as usize).then_some(span)
}

/// Copy the pixels of a mapped image into tightly packed rows.
///
/// # Safety
///
/// For a valid layout, each row starting at `base + row * pitch` must contain
/// `row_bytes` initialized bytes in the same allocation. Padding may be uninitialized.
pub(crate) unsafe fn pack_rows(
    base: *const u8,
    pitch: usize,
    row_bytes: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let mut packed = Vec::new();
    unsafe { append_rows(&mut packed, base, pitch, row_bytes, height)? };
    Some(packed)
}

/// Append the initialized bytes from each row to an existing frame.
///
/// # Safety
///
/// The source must satisfy the same layout requirements as [`pack_rows`]
/// and must not alias `packed`.
pub(crate) unsafe fn append_rows(
    packed: &mut Vec<u8>,
    base: *const u8,
    pitch: usize,
    row_bytes: usize,
    height: usize,
) -> Option<()> {
    let span = row_span(pitch, row_bytes, height)?;
    if base.is_null() {
        return None;
    }
    unsafe {
        if pitch == row_bytes {
            packed.extend_from_slice(core::slice::from_raw_parts(base, span));
        } else {
            packed.reserve(row_bytes * height);
            for row in 0..height {
                packed.extend_from_slice(core::slice::from_raw_parts(
                    base.add(row * pitch),
                    row_bytes,
                ));
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_row_needs_only_its_pixels() {
        let bytes = [1, 2, 0, 0, 3, 4];
        assert_eq!(row_span(4, 2, 2), Some(bytes.len()));
        assert_eq!(
            unsafe { pack_rows(bytes.as_ptr(), 4, 2, 2) }.unwrap(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            unsafe { pack_rows(bytes.as_ptr(), 2, 2, 3) }.unwrap(),
            bytes
        );
    }

    #[test]
    fn rejects_invalid_row_layouts() {
        assert_eq!(row_span(1, 2, 1), None);
        assert_eq!(row_span(2, 2, 0), None);
        assert_eq!(row_span(usize::MAX, 2, 2), None);
        assert_eq!(row_span(isize::MAX as usize, 2, 2), None);
    }
}
