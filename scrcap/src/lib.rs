#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

pub mod capture_desc;
pub mod config;
pub mod error;
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
pub use frame::*;
#[cfg(not(feature = "dummy"))]
pub use platform::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn capture_test() {
        let config = CaptureConfig {
            channel_capacity: 2,
            video: VideoConfig {
                hide: vec![],
                target: Target::Primary,
            },
            audio: Some(AudioConfig {}),
        };
        let capture_desc = config.create().expect("Failed to create capture");
        let mut nb_frames = 0;
        let mut total_nb_samples = 0;
        let now = std::time::Instant::now();
        for i in 0..300 {
            println!("{i}");
            let frame = capture_desc.recv_timeout(Duration::from_secs(4)).unwrap();
            match frame {
                Frame::Video {
                    vframe,
                    size: (width, height),
                    ..
                } => {
                    assert_eq!(vframe.len(), (width * height * 4) as usize);
                    nb_frames += 1;
                }
                Frame::Audio { nb_samples, .. } => {
                    total_nb_samples += nb_samples;
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
}
