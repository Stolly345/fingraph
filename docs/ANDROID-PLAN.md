# FinGraph on Android: plan

Written 9 November 2026, against 0.2.30. A plan, not a promise: estimates are rough and assume one
developer working with an AI agent.

## 1. What it should be

A phone companion to the desktop ledger, not a second, separate ledger. The phone is where you are
when you spend money and when the bank's app shows a new statement; the desktop is where the monthly
close, setup and reports belong. So the phone should, in this order:

1. **Show** the budget, what's left, bills due, subscriptions and the next steps, from the same ledger.
2. **Capture** what the desktop can't: a cash purchase, a receipt photo, a statement shared from the
   bank's app. This also closes the PRD's long-standing gap (*manual transaction entry*, so the current
   week counts before the statement arrives).
3. Later, **import and review** on the phone itself.

Everything stays local-first: no FinGraph server, no bank login, nothing sent anywhere.

## 2. What the code allows (audit)

| Part | Android-ready? | Notes |
|---|---|---|
| `fingraph-core` (17k lines) | **Mostly.** | Pure Rust: SQLite is bundled (`rusqlite` with `bundled`), money is `rust_decimal`, no GUI. Builds for `aarch64-linux-android` once the items below are handled. |
| Scanner (`parser::MultiPassScanner::scan_tokens`) | **Yes.** | Works from a list of word tokens with boxes (`Token { page, x0, y0, x1, y1, text }`), independent of how they were read. |
| Reading PDFs (`tokens_from_pdf`, `page_tokens`) | **No.** | Runs `pdftotext -bbox` and `pdfinfo` (poppler) as programs. Android has neither. |
| OCR (`ocr_tokens`) | **No.** | Runs `pdftoppm` and `tesseract`. |
| Exchange rates (`rates.rs`) | **No.** | Runs `curl`. |
| Paths (`lib.rs`: `XDG_DATA_HOME`, `$HOME`; parser scratch in `XDG_RUNTIME_DIR`) | **Adjust.** | Android gives the app its own files and cache folders; the host app must pass them in. |
| Import (`Db::import_auto`), backups (`backup.rs`, TAR) | **Yes.** | The complete backup format is the natural way to move a ledger between devices. |
| Desktop UI (`crates/fingraph`, GTK4/libadwaita, 16k lines) | **No.** | Rewritten for the phone; it's the bulk of the work. |

Toolchain on this machine: `cargo-ndk` and the `aarch64-linux-android` / `x86_64-linux-android` Rust
targets are installed, and `adb` reaches an arm64-v8a test phone over Wi-Fi. Missing: a JDK, the
Android SDK command-line tools and platform, the NDK, and Gradle. Flutter is installed (snap) but isn't
the recommendation below.

## 3. Decisions

### UI: Kotlin and Jetpack Compose, with the Rust core through UniFFI

| Option | For | Against |
|---|---|---|
| **Kotlin + Compose + UniFFI** (recommended) | Native Material 3 feel; first-class share sheet, camera, file picker, biometric lock, WorkManager and on-device OCR (ML Kit); UniFFI generates the Kotlin bindings from Rust. | Android only (an iPhone app would need SwiftUI, though UniFFI generates Swift too). |
| Flutter + flutter_rust_bridge | One UI for Android and iOS; Flutter is already installed. | Another language (Dart) and widget set; platform features through plugins. |
| Tauri 2 mobile | Web UI. | The desktop UI isn't web, so nothing is reused; heaviest runtime. |
| GTK on Android | Reuse the desktop UI. | Not practical. |

### Core: one library, platform pieces behind small traits

- `TokenSource`: *poppler* on the desktop (today's code, unchanged), *PDFium* on Android
  (`pdfium-render` with the prebuilt `libpdfium.so` for arm64). It gives character boxes, grouped
  into words, converted to the same top-left, points-based coordinates `pdftotext -bbox` produces, so every
  bank profile works unchanged.
- `Ocr`: *tesseract* on the desktop; on Android the app hands page images to ML Kit Text Recognition
  (on-device, offline) and returns tokens to Rust.
- `Http` for exchange rates: `curl` on the desktop, `ureq` with rustls on Android (or a Kotlin callback).
- Paths are passed in by the host: `Db::open(path)` already takes one; add a `set_data_dir` for the
  statements and attachments folders and the scratch folder.
- Cargo features (`desktop`, `android`) choose the backends, so the desktop build doesn't change.

### FFI: a narrow facade, not 438 functions

A new crate `crates/fingraph-ffi` exposes **screen-shaped** calls that return plain records, for example
`budget_overview(month)`, `transactions(filter, page)`, `triage_cards()`, `answer_card(...)`,
`subscriptions()`, `outlook()`, `next_steps()`, `add_manual(...)`, `import_pdf(path)`. The view-model
logic that lives in GTK files today (Summary's Next Steps, the budget snapshot) moves into the core, so
desktop and phone share one source of truth. `Db` isn't thread-safe, so the facade owns it on one worker
thread and the Kotlin side calls it from a coroutine dispatcher.

### Data between devices: complete backups first, a journal second, sync last

Two devices writing the same SQLite file is how ledgers get corrupted. So, in order:

1. **Phone reads a copy.** The desktop already writes complete backups (ledger + statements + receipts +
   SHA-256 manifest). The phone opens the newest one from a folder you choose: Syncthing, a home server over
   a VPN, or a manual share. Read-only, nothing to conflict.
2. **Phone captures into a journal.** Manual purchases, receipt photos and shared statements go into an
   append-only *inbox* file (JSON lines plus attachments) beside the backup. The desktop imports the inbox
   at the next start (manual rows, receipts attached to their transactions, statements through the normal
   import with its reconciliation and review holds) and marks it consumed. One writer per file: no conflicts.
3. **Two-way sync** (optional, much later): only if 1 and 2 aren't enough, as row-level change sets keyed by
   the existing transaction fingerprints, with the desktop as the authority.

### Security

The ledger is a full financial history, so on the phone: an app lock (biometric or PIN through
`BiometricPrompt`); the database encrypted at rest (SQLCipher through `rusqlite`'s `bundled-sqlcipher`,
key held in the Android Keystore); `allowBackup="false"` so it never goes to Google's cloud backup;
screenshots blocked on sensitive screens (`FLAG_SECURE`, with a setting to allow). No network access except
exchange-rate downloads, which you can switch off as on the desktop.

## 4. Phases

| Phase | Delivers | Main work | Rough size |
|---|---|---|---|
| **0. Toolchain** | An empty Compose app calling one Rust function on the test phone. | JDK 17, SDK cmdline-tools + platform 35, NDK r27, Gradle wrapper; `cargo ndk -t arm64-v8a`; UniFFI hello-world; `adb install`. | 1 day |
| **1. Portable core** | `fingraph-core` builds for Android; desktop unchanged. | `TokenSource` / `Ocr` / `Http` traits and features; path injection; a **parity test** that reads every sample and demo PDF with both poppler and PDFium and requires the same transactions and reconciliation. | 3–5 days |
| **2. Read-only companion** | Budget, Transactions (search, detail), Subscriptions status, Outlook summary, Next Steps, from a backup. | `fingraph-ffi` facade; move Next Steps and the budget snapshot into the core; five Compose screens; backup picker; app lock. | 2–3 weeks |
| **3. Capture** | Quick-add purchase, receipt camera, *Share PDF to FinGraph* from the bank app; desktop imports the inbox. | Inbox format and desktop importer (with tests); CameraX; share intent; Material date and amount entry. | 1–2 weeks |
| **4. Review on the phone** | To Review cards (swipe to answer, with the same explanations), rule preview, refund links. | Triage facade; card UI; the inbox carries answers too, applied on the desktop as rules. | 1–2 weeks |
| **5. Import on the phone** (optional) | Statements read and proven on the phone. | PDFium and ML Kit backends live; reconciliation and review holds as on the desktop; SQLCipher; the phone keeps its own ledger, merged through the inbox. | 2–3 weeks |
| **6. Polish and release** | Signed APK on the test phone, then GitHub releases. | Release signing key kept outside the public repo; R8; size check (PDFium adds about 5 MB); dark theme; tablet layout; README section. | 1 week |

Phases 0–3 give a useful companion (about 4–6 weeks). Each phase ends with tests passing, the APK
installed on the test phone over `adb`, and nothing changed for desktop users.

## 5. Risks

- **PDF coordinates.** Bank profiles depend on column positions. PDFium and poppler disagree about
  origin and rounding. The phase 1 parity test is the guard; if a bank can't match, the phone defers
  that bank's statements to the desktop (the inbox carries the PDF).
- **OCR quality.** ML Kit and tesseract read differently; picture-only statements already go to review on
  the desktop and would on the phone too.
- **Two copies of the truth.** The read-only plus inbox design avoids it; resist "just sync the SQLite file".
- **Effort is in the UI.** The core is shared; every screen is new. Keep the phone to the screens above.
- **Public repo.** Signing keys, test ledgers and real statements never go in it (as with merchant hints).

## 6. Questions for you

1. Read-only companion first (recommended), or the phone importing statements on its own from the start?
2. How should backups reach the phone: Syncthing, a home server over a VPN, or sharing a file by hand?
3. Android only, or could an iPhone matter later (it would tip the choice toward Flutter)?
4. Sideload to your own phone only, or publish APKs on GitHub releases like the desktop .deb?
