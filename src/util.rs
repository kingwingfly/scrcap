//! Small helpers shared by the platform backends.

#[cfg(any(target_os = "linux", test))]
use core::mem::MaybeUninit;

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

/// Copy the pixels of a mapped BGRx image into tightly packed rows, writing `0xFF`
/// into each pixel's undefined 4th byte so the source padding byte is never read as `u8`.
///
/// # Safety
///
/// For a valid layout, each row starting at `base + row * pitch` must lie within a
/// single allocation of at least `width * 4` bytes, with the first 3 bytes (B, G, R)
/// of every 4-byte pixel initialized. Each pixel's 4th byte and any row padding may
/// be uninitialized.
#[cfg(any(target_os = "linux", test))]
pub(crate) unsafe fn pack_rows_bgr0(
    base: *const u8,
    pitch: usize,
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(4)?;
    let _span = row_span(pitch, row_bytes, height)?;
    if base.is_null() {
        return None;
    }
    let total = row_bytes.checked_mul(height)?;
    let mut packed = Vec::with_capacity(total);
    let dst = &mut packed.spare_capacity_mut()[..total];
    for row in 0..height {
        let src_row = unsafe {
            core::slice::from_raw_parts(base.add(row * pitch).cast::<MaybeUninit<u8>>(), row_bytes)
        };
        let dst_row = &mut dst[row * row_bytes..(row + 1) * row_bytes];
        for (src_px, dst_px) in src_row
            .as_chunks::<4>()
            .0
            .iter()
            .zip(dst_row.as_chunks_mut::<4>().0)
        {
            dst_px[0] = src_px[0];
            dst_px[1] = src_px[1];
            dst_px[2] = src_px[2];
            dst_px[3] = MaybeUninit::new(0xFF);
        }
    }
    // Safety: Every pixel's B/G/R bytes came from initialized source bytes per caller
    // contract, and every 4th byte was explicitly written with `0xFF`.
    unsafe { packed.set_len(total) };
    Some(packed)
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

    #[test]
    fn pack_rows_bgr0_leaves_uninit_padding_unread_and_sets_alpha_ff() {
        // 2x2 BGRx image with 12-byte pitch (4 bytes trailing row padding) where the
        // 4th byte of every pixel and the row padding are genuinely uninitialized,
        // and the last row has no trailing padding beyond its pixels.
        let mut buf = [MaybeUninit::<u8>::uninit(); 12 + 8];
        let write_bgr = |buf: &mut [MaybeUninit<u8>], offset: usize, bgr: [u8; 3]| {
            for (i, b) in bgr.into_iter().enumerate() {
                buf[offset + i].write(b);
            }
        };
        write_bgr(&mut buf, 0, [10, 20, 30]);
        write_bgr(&mut buf, 4, [40, 50, 60]);
        write_bgr(&mut buf, 12, [70, 80, 90]);
        write_bgr(&mut buf, 16, [100, 110, 120]);

        let packed = unsafe { pack_rows_bgr0(buf.as_ptr().cast(), 12, 2, 2) }.unwrap();
        assert_eq!(
            packed,
            [
                10, 20, 30, 0xFF, 40, 50, 60, 0xFF, // row 0
                70, 80, 90, 0xFF, 100, 110, 120, 0xFF, // row 1
            ]
        );

        // Tight layout (pitch == width * 4) with uninitialized 4th bytes.
        let mut tight = [MaybeUninit::<u8>::uninit(); 8];
        write_bgr(&mut tight, 0, [1, 2, 3]);
        write_bgr(&mut tight, 4, [4, 5, 6]);
        let packed = unsafe { pack_rows_bgr0(tight.as_ptr().cast(), 8, 2, 1) }.unwrap();
        assert_eq!(packed, [1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);

        // Rejects invalid dimensions, pitch, overflow, and null pointer.
        assert_eq!(unsafe { pack_rows_bgr0(core::ptr::null(), 8, 2, 1) }, None);
        assert_eq!(
            unsafe { pack_rows_bgr0(tight.as_ptr().cast(), 8, 0, 1) },
            None
        );
        assert_eq!(
            unsafe { pack_rows_bgr0(tight.as_ptr().cast(), 8, 2, 0) },
            None
        );
        assert_eq!(
            unsafe { pack_rows_bgr0(tight.as_ptr().cast(), 4, 2, 1) },
            None
        );
        assert_eq!(
            unsafe { pack_rows_bgr0(tight.as_ptr().cast(), 8, usize::MAX, 1) },
            None
        );
    }
}
