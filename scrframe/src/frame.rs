//! frame traits

pub trait VFrame: AsRef<[u8]> {
    fn size(&self) -> (u32, u32);
    /// Returns the pixel format as an i32 ID in ffmpeg.
    ///
    /// https://docs.rs/rsmpeg/latest/rsmpeg/?search=AV_PIX_FMT
    fn pix_fmt(&self) -> i32;
    /// If `Some(ts)`(in ms) returned, the inner ts will be used to compute pts,
    /// and then, the pts will be used to replace the one in encoder.
    /// ```no_run
    /// let pts = (ts * v_encoder.framerate.num as u128
    ///    / v_encoder.framerate.den as u128
    ///    / 1000) as i64;
    /// pts += *v_pts_offset.get_or_insert(-pts);
    /// ```
    /// The pts computed from the first ts will be used as an offset to make pts start at 0
    fn ts(&self) -> Option<u128> {
        None
    }
}

pub trait AFrame: AsRef<[u8]> {
    fn nb_samples(&self) -> i32;
    fn sample_rate(&self) -> i32;
    fn nb_channels(&self) -> i32;
    /// Returns the sample format as an i32 ID in ffmpeg.
    fn sample_fmt(&self) -> i32;
}
