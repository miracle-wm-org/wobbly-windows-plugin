# wobbly-windows

A miracle-wm plugin that makes windows wobble as they slide to a new position,
then settle.

The compositor's built-in move animation still moves the window. The plugin reads
where that animation puts the window on every frame, drives a damped spring from
it, and hands the spring's lag to a geometry shader that bends the window (content
and border together).

## Requirements

- An OpenGL ES 3.2 context (geometry shaders). Without one the compositor logs a
  warning and windows move normally.
- Animations enabled, with a `window_move` animation configured (e.g. `slide`).

## Installation

Download and install the latest stable release:

```sh
curl -fsSL https://raw.githubusercontent.com/miracle-wm-org/wobbly-windows-plugin/main/install.sh | bash
```

Or the latest nightly build:

```sh
curl -fsSL https://raw.githubusercontent.com/miracle-wm-org/wobbly-windows-plugin/main/install.sh | bash -s -- --nightly
```

This installs the plugin to `~/.config/miracle-wm/plugins`, where miracle-wm loads
it automatically.

Alternatively, manually add the plugin to your miracle-wm configuration file
(`~/.config/miracle-wm/config.yaml`):

```yaml
# ~/.config/miracle-wm/config.yaml

plugins:
  - path: /path/to/wobbly-windows-plugin/target/wasm32-wasip1/release/wobbly_windows.wasm
```

## Building

### Prerequisites

```sh
rustup target add wasm32-wasip1
```

### Compilation

```sh
cargo build --target wasm32-wasip1 --release
```

The compiled WASM file can be found at `target/wasm32-wasip1/release/wobbly_windows.wasm`.
