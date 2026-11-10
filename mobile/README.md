# FinGraph for Android

The phone companion to the desktop ledger, through [FinGraph Server](../server/README.md): the month's
budget and what's left, bills, subscriptions, accounts, next steps and every transaction, kept for when
the server can't be reached; quick purchases, receipt photos and statement PDFs (Share → FinGraph) go
to the server. See [`docs/SERVER-PLAN.md`](../docs/SERVER-PLAN.md).

It's a Flutter app (Android only for now; nothing in it rules out an iPhone build later). It talks to
the server over HTTPS and trusts only the certificate it paired with (`lib/src/connection.dart`).
The FinGraph core is also built in through [`crates/fingraph-ffi`](../crates/fingraph-ffi) and
`flutter_rust_bridge`, for a later mode that works without a server.

| File | What |
|---|---|
| `lib/src/connection.dart` | Pairing, the pinned HTTPS client, the saved server (Android Keystore) |
| `lib/src/data.dart` | The month and workspace on screen, views with their offline copies, formatting |
| `lib/src/lock.dart` | App lock and hiding from screenshots |
| `lib/src/capture.dart` | Statement PDFs, quick purchases, receipt photos |
| `lib/src/pages/` | Connect, Summary, Budget, Transactions, More (Bills, Subscriptions, Accounts, Settings) |
| `android/.../MainActivity.kt` | Screenshot blocking, receiving shared PDFs, the file picker |

## Build

Needs Flutter, the Android SDK and NDK, JDK 17, Rust with the `aarch64-linux-android` target, and
`flutter_rust_bridge_codegen` 2.11.1 (`cargo install flutter_rust_bridge_codegen --version 2.11.1`).

```sh
cd mobile
flutter test
flutter_rust_bridge_codegen generate   # after changing crates/fingraph-ffi/src/api
flutter build apk --release --target-platform android-arm64
adb install -r build/app/outputs/flutter-apk/app-release.apk
```

## Signing

Release builds are signed with the key named in `~/.config/fingraph-android/key.properties` (or the
file in `FINGRAPH_ANDROID_KEYS`), which is never committed. Without it they fall back to the debug key,
and such an APK can't be installed over a release-signed one. Every APK published on GitHub and installed
on the owner's phone uses the same release key, so each version installs over the last.
