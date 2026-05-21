# hypermakey

`hypermakey` is a tiny Apple Silicon macOS background utility that turns Caps Lock into a held Hyper key: Control + Option + Shift + Command.

It is designed to be minimal:

- Rust single-binary daemon
- native macOS event APIs
- event-driven, no polling
- no GUI, menu bar item, network, telemetry, root daemon, or kernel extension

## Current behavior

- Hold Caps Lock to press Hyper (`⌃⌥⇧⌘`).
- Release Caps Lock to release Hyper.
- Suppress the original Caps Lock event so it should not toggle caps state.
- Release synthesized modifiers during shutdown and event tap recovery to avoid stuck keys.

## Build

```sh
cargo build --release --target aarch64-apple-darwin
```

## Run manually

```sh
cargo run --release
```

macOS requires Accessibility permission. If startup fails or Caps Lock does nothing, grant permission here:

- System Settings → Privacy & Security → Accessibility

For the Homebrew service, the executable path is typically:

```text
/opt/homebrew/opt/hypermakey/bin/hypermakey
```

After changing permissions, restart `hypermakey`.

## Homebrew service

The intended install path is Homebrew plus `brew services` through the `stellarjmr/tool` tap.

Install and run it as a user service:

```sh
brew install stellarjmr/tool/hypermakey
brew services start stellarjmr/tool/hypermakey
```

If you grant permissions after the service has already started, restart it:

```sh
brew services restart stellarjmr/tool/hypermakey
```

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --target aarch64-apple-darwin
```

Manual checks:

- Holding Caps Lock alone does not toggle caps state.
- Caps Lock + another key is recognized as `⌃⌥⇧⌘` + that key.
- Releasing Caps Lock does not leave modifiers stuck.
- Idle CPU usage is near 0.
