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
- Stop instead of re-enabling the event tap if Accessibility permission was revoked while running, so keyboard input is not stalled again and again.
- Read only the Caps Lock element from keyboard HID devices; other key values are never delivered to hypermakey through the HID path.
- Prefix log lines with the local time and UTC offset.

## Build

```sh
cargo build --release --target aarch64-apple-darwin
```

## Run manually

```sh
cargo run --release
```

macOS requires Accessibility permission, and Input Monitoring if macOS asks for it. If a permission is missing, hypermakey exits with a message naming the permission and the binary to grant it to:

- System Settings → Privacy & Security → Accessibility
- System Settings → Privacy & Security → Input Monitoring

When hypermakey is started from a terminal, macOS checks the terminal app's permissions instead of hypermakey's. A terminal with Accessibility permission lets every program started from it monitor and inject input, so prefer the Homebrew service for everyday use and revoke the terminal's permission after testing.

For the Homebrew service, the executable path is typically:

```text
/opt/homebrew/opt/hypermakey/bin/hypermakey
```

After changing permissions, restart `hypermakey`.

After an upgrade, macOS may deny the permission to the new binary. hypermakey then logs the missing permission, and macOS lists hypermakey in the Accessibility pane switched off: switch it back on.

Do not remove hypermakey from the list or switch it off while it is running. Stop the service first (`brew services stop stellarjmr/tool/hypermakey`): an event tap whose permission is revoked stalls keyboard input until macOS times it out.

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

The service runs with launchd `ProcessType` `Interactive`. Every keystroke waits for hypermakey's event tap, so launchd must not throttle it like a background job. Check the scheduling priority after a restart (31 or higher; 20 means the job is still throttled):

```sh
ps -o pri= -p "$(pgrep -x hypermakey)"
```

## Release

Pushing a `v*` tag runs `.github/workflows/release.yml`:

1. `scripts/package-release.sh` builds the binary, signs it ad hoc with the hardened runtime, verifies the signature, and writes `dist/hypermakey-<tag>-aarch64-apple-darwin.tar.gz` with its SHA-256. The tag must equal `v` plus the version in `Cargo.toml`.
2. A build provenance attestation is generated for the archive and the binary.
3. The archive is published as a GitHub release, and the job summary shows the SHA-256 for the Homebrew formula.

The hardened runtime makes dyld ignore `DYLD_*` environment variables, so no other code can be injected into a process that holds Accessibility permission. No entitlements are needed.

Verify a downloaded archive or an installed binary:

```sh
gh attestation verify hypermakey-<tag>-aarch64-apple-darwin.tar.gz --repo stellarjmr/hypermakey
gh attestation verify /opt/homebrew/opt/hypermakey/bin/hypermakey --repo stellarjmr/hypermakey
codesign --display --verbose /opt/homebrew/opt/hypermakey/bin/hypermakey  # flags include "runtime"
```

Verifying the installed binary works because Homebrew installs it byte-for-byte unchanged; if Homebrew ever re-signs it on install, verify the archive instead.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --target aarch64-apple-darwin
scripts/package-release.sh
```

CI runs these checks plus `cargo audit` on every push and pull request, and weekly.

Manual checks:

- Holding Caps Lock alone does not toggle caps state.
- Caps Lock + another key is recognized as `⌃⌥⇧⌘` + that key.
- Releasing Caps Lock does not leave modifiers stuck.
- Idle CPU usage is near 0.
