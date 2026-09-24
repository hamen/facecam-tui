# facecam-tui

Terminal UI for the Elgato Facecam (USB `0fd9:0078`) on Linux: live preview, a realtime
exposure slider, brightness, and Auto / Shutter Priority.

The Linux `uvcvideo` driver keeps the Facecam's exposure time locked, because it only allows
that control in "Manual" mode and the Facecam has no Manual mode. This tool writes the exposure
time straight to the camera with a UVC request over usbfs, which works while the camera streams.

## Development

After you clone, enable the pre-push gate:

    git config extensions.worktreeConfig true
    git config --worktree core.hooksPath .githooks

`bin/ci` runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`.
