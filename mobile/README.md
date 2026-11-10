# FinGraph for Android

The phone companion to the desktop ledger. See [`docs/ANDROID-PLAN.md`](../docs/ANDROID-PLAN.md) for
the plan and its phases. Today (phase 0) it only proves the core runs on the phone: it scans the demo
statements into a temporary ledger and shows what it found.

It's a Flutter app. The FinGraph core is the same Rust code as the desktop, reached through
[`crates/fingraph-ffi`](../crates/fingraph-ffi) and `flutter_rust_bridge`.

## Build

Needs Flutter, the Android SDK and NDK, JDK 17, Rust with the `aarch64-linux-android` target, and
`flutter_rust_bridge_codegen` 2.11.1 (`cargo install flutter_rust_bridge_codegen --version 2.11.1`).

```sh
cd mobile
flutter_rust_bridge_codegen generate   # after changing crates/fingraph-ffi/src/api
flutter build apk --release --target-platform android-arm64
adb install -r build/app/outputs/flutter-apk/app-release.apk
```

## Signing

Release builds are signed with the key named in `~/.config/fingraph-android/key.properties` (or the
file in `FINGRAPH_ANDROID_KEYS`), which is never committed. Without it they fall back to the debug key,
and such an APK can't be installed over a release-signed one. Every APK published on GitHub and installed
on the owner's phone uses the same release key, so each version installs over the last.
