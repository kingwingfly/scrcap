#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

pub mod capture_desc;
pub mod config;
pub mod error;
pub mod format;
mod fps;
pub mod frame;

#[cfg(feature = "dummy")]
pub mod dummy;
#[cfg(not(feature = "dummy"))]
#[cfg_attr(target_os = "windows", path = "windows/mod.rs")]
#[cfg_attr(target_os = "macos", path = "macos/mod.rs")]
#[cfg_attr(target_os = "linux", path = "linux/mod.rs")]
pub mod platform;

pub use capture_desc::*;
pub use config::*;
#[cfg(feature = "dummy")]
pub use dummy::*;
pub use format::*;
pub use frame::{AudioFrame, VideoFrame};
#[cfg(not(feature = "dummy"))]
pub use platform::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::select;
    use std::time::Duration;

    #[test]
    #[cfg_attr(
        not(feature = "dummy"),
        ignore = "opens a real capture session; needs a display, run with --ignored"
    )]
    fn capture_test() {
        let config = CaptureConfig {
            video: VideoConfig {
                channel_capacity: 2,
                hide: vec![],
                target: Target::Primary,
                fps: Some(60),
            },
            audio: Some(AudioConfig {
                channel_capacity: 64,
            }),
        };
        let capture_desc = config.create().expect("Failed to create capture");
        let mut nb_frames = 0;
        let mut total_nb_samples = 0;
        let mut last_ts = 0;
        let now = std::time::Instant::now();
        for i in 0..300 {
            println!("{i}");
            match capture_desc.audio() {
                Some(audio) => select! {
                    recv(capture_desc.video()) -> frame => {
                        let frame = frame.unwrap();
                        let (width, height) = frame.size;
                        assert_eq!(frame.vframe.len(), (width * height * 4) as usize);
                        check_ts(frame.ts, &mut last_ts);
                        nb_frames += 1;
                    }
                    recv(audio) -> frame => {
                        let frame = frame.unwrap();
                        check_ts(frame.ts, &mut last_ts);
                        total_nb_samples += frame.nb_samples;
                    }
                    default(Duration::from_secs(4)) => panic!("no frame within 4 s"),
                },
                None => {
                    let frame = capture_desc
                        .video()
                        .recv_timeout(Duration::from_secs(4))
                        .unwrap();
                    let (width, height) = frame.size;
                    assert_eq!(frame.vframe.len(), (width * height * 4) as usize);
                    check_ts(frame.ts, &mut last_ts);
                    nb_frames += 1;
                }
            }
        }
        capture_desc.terminate();
        println!(
            "frames: {nb_frames}; fps: {} hz",
            nb_frames as f32 / now.elapsed().as_secs_f32()
        );
        println!(
            "samples: {total_nb_samples}; sample rate: {} hz",
            total_nb_samples as f32 / now.elapsed().as_secs_f32()
        );
    }

    /// Video and audio share one monotonic clock, so one running maximum covers both.
    fn check_ts(ts: u64, last: &mut u64) {
        assert!(ts > 0, "frame has no timestamp");
        assert!(ts >= *last, "timestamp went backwards: {ts} after {last}");
        *last = ts;
    }
}
