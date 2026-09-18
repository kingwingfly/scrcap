# About

A screen capture crate for Windows/macOS/Linux.

The docs.rs pages cover the Windows and macOS backends only: the Linux one binds PipeWire,
whose headers the docs.rs build image does not carry, so `cargo doc` locally is the only way
to read `platform` for Linux.

# Usage

A capture is a [`CaptureConfig`] you `create()`, and a [`CaptureDescriptor`] you pull frames
from. Video and audio get a channel each.

## Seeing it work

The quickest way to watch a capture is the `winit` example, a preview window that draws the
captured frames into itself:

```sh
cargo run --example winit
```

It opens an always-on-top window, waits until that window has drawn a frame of its own, and
only then raises the system picker. Pick a source and it appears in the window; `Esc` quits. 
On Windows and macOS the preview **hides itself** from the capture through [`VideoConfig::hide`],
on Linux no protocol allows that, so expect exactly that.

For somewhere with no display and no picker, `cargo test --features dummy` swaps the whole
platform module for a fake backend that generates frames, and runs the capture test headless.

## Video only

`size()` is known by the time `create` returns; every frame carries its own size, pixel
format and capture timestamp anyway.

```rust,no_run
use scrcap::{CaptureConfig, CaptureDescriptor as _, Target, VideoConfig, error::Result};

fn main() -> Result<()> {
    let capture = CaptureConfig {
        video: VideoConfig {
            // At least 1. A full channel drops frames rather than blocking the capture.
            channel_capacity: 2,
            // Window ids to keep out of the recording (`HWND` / `CGWindowID`).
            hide: vec![],
            target: Target::Primary,
            // A cap, not a target: it cannot raise a rate the machine cannot sustain.
            fps: Some(60),
        },
        audio: None,
    }
    .create()?;

    let (width, height) = capture.size();
    println!("capturing {width}x{height}");

    // The origin of `ts` is platform defined, so subtract the first one of the session.
    let mut start = None;
    for _ in 0..300 {
        // `Err` only once the capture thread is gone.
        let frame = capture.video().recv().expect("the capture ended");
        let start = *start.get_or_insert(frame.ts);
        // BGRA on Windows and macOS, BGRA *or* BGRx on Linux -- read `pix_fmt`, do not
        // assume. Either way `bytes_per_pixel() * width * height` bytes.
        println!(
            "{:?} {}x{} at {} ms",
            frame.pix_fmt,
            frame.size.0,
            frame.size.1,
            (frame.ts - start) / 1_000_000
        );
    }

    // `Drop` terminates too; this just does it early.
    capture.terminate();
    Ok(())
}
```

## Video and audio

Two channels means two receivers to drain, so take whichever has a frame ready. Give audio
the deeper capacity of the two. `sample_rate()` answers `None` until the system has settled
the format, which on Linux and macOS happens after `create` returns -- every [`AudioFrame`]
carries its own rate, so the first frame always answers it.

```rust,no_run
use crossbeam_channel::select;
use scrcap::{AudioConfig, CaptureConfig, CaptureDescriptor as _, Target, VideoConfig, error::Result};

fn main() -> Result<()> {
    let capture = CaptureConfig {
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
    let capture = capture.create()?;
    let audio = capture.audio().expect("audio was configured");

    loop {
        select! {
            recv(capture.video()) -> frame => match frame {
                Ok(frame) => println!("video: {} bytes", frame.vframe.len()),
                Err(_) => break, // the capture ended
            },
            recv(audio) -> frame => match frame {
                Ok(frame) => println!(
                    "audio: {} samples of {:?} at {} Hz",
                    frame.nb_samples, frame.sample_fmt, frame.sample_rate
                ),
                Err(_) => break,
            },
        }
    }
    Ok(())
}
```

## Letting the user choose

`Target::Pick` raises the system picker and blocks until the user has chosen, so it must not
run on the thread driving the UI: the Windows picker is parented to the `HWND` you pass, whose
message pump would be the one blocked, and the macOS picker answers on the main queue. Linux
always shows the portal's picker, whatever the target, and only `Primary`/`Monitor` (monitors
only) and `Pick` (everything) narrow what it offers.

```rust,no_run
use std::thread;
use scrcap::{CaptureConfig, CaptureDesc, Target, VideoConfig, error::Result};

fn pick(
    #[cfg(target_os = "windows")] parent_hwnd: isize,
) -> thread::JoinHandle<Result<CaptureDesc>> {
    // Only `GraphicsCapturePicker` needs a parent window, so `Pick` carries one on Windows
    // and is a unit variant elsewhere.
    #[cfg(target_os = "windows")]
    let target = Target::Pick(parent_hwnd);
    #[cfg(not(target_os = "windows"))]
    let target = Target::Pick;

    thread::spawn(move || {
        CaptureConfig {
            video: VideoConfig {
                channel_capacity: 2,
                hide: vec![],
                target,
                fps: Some(60),
            },
            audio: None,
        }
        .create()
    })
}
```

## Errors worth matching

[`CaptureError`](error::CaptureError) carries no free-form strings: every case a caller can act on is a variant.
A missing capability is [`Unsupported`](error::Unsupported), which is usually worth retrying without the feature
that asked for it -- audio needs macOS 13+, and on Linux a connection to the PipeWire daemon
that a sandbox holding only the portal's screencast remote does not have.

```rust,no_run
use scrcap::{
    AudioConfig, CaptureConfig, CaptureDesc, Target, VideoConfig,
    error::{CaptureError, Result, Unsupported},
};

/// `Ok(None)` when the user changed their mind; everything else is a real failure.
fn create() -> Result<Option<CaptureDesc>> {
    let config = |audio: bool| CaptureConfig {
        video: VideoConfig {
            channel_capacity: 2,
            hide: vec![],
            target: Target::Primary,
            fps: Some(60),
        },
        audio: audio.then(|| AudioConfig {
            channel_capacity: 64,
        }),
    };

    match config(true).create() {
        // This machine cannot capture audio at all; the video alone still works.
        Err(CaptureError::Unsupported(Unsupported::Audio)) => config(false).create().map(Some),
        // The user dismissed the picker or the permission prompt: not a malfunction,
        // so do not re-raise the picker in their face.
        Err(CaptureError::Cancelled) => Ok(None),
        // macOS only, and no retry will clear it: Screen Recording has to be granted in
        // System Settings > Privacy & Security, and only takes effect after a restart.
        Err(e @ CaptureError::PermissionDenied) => {
            eprintln!("grant Screen Recording, then restart this app");
            Err(e)
        }
        result => result.map(Some),
    }
}
```

To restart a running capture with a different configuration, use
[`CaptureDescriptor::update_config`] rather than creating a second one: it takes the old
descriptor by value and drops it first, so the old capture cannot restore the very windows
the new one just hid.

# Features

- `dummy`: A dummy implementation that returns random 1080x720 images (`AV_PIX_FMT_BGRA`). No audio is produced.

# AI Involvement

In this repo, commits by @kingwingfly are manually coded, while those by @kingwingfly-ai are coded by agents (Claude + OpenAI).
