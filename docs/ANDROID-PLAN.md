# FinGraph on Android: plan

Written 9 November 2026, against 0.2.30. A plan, not a promise: estimates are rough and assume one
developer working with an AI agent.

**Decided (9 November 2026):** a read-only companion first; backups reach the phone through Syncthing,
with the always-on home server as an encrypted (untrusted) peer with file versioning; **Flutter** for the
app, Android only for now, so an iPhone version stays possible later; installed on the owner's phone and
published as APKs on GitHub releases, both signed with one release key.

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

### UI: Flutter, with the Rust core through flutter_rust_bridge (decided)

An iPhone version may matter later, so one UI for both phones outweighs the native feel of Compose. Flutter is
free and open source (BSD). Phone features come through maintained plugins (camera, receiving shared files,
biometric lock, ML Kit text recognition), with small Kotlin pieces where a plugin is awkward. An iPhone build
would still need a Mac and Apple's developer programme; nothing in the plan depends on it.

The comparison as written before the decision:

| Option | For | Against |
|---|---|---|
| Kotlin + Compose + UniFFI | Native Material 3 feel; first-class share sheet, camera, file picker, biometric lock, WorkManager and on-device OCR (ML Kit); UniFFI generates the Kotlin bindings from Rust. | Android only (an iPhone app would need SwiftUI, though UniFFI generates Swift too). |
| **Flutter + flutter_rust_bridge** (chosen) | One UI for Android and iOS; Flutter is already installed. | Another language (Dart) and widget set; platform features through plugins. |
| Tauri 2 mobile | Web UI. | The desktop UI isn't web, so nothing is reused; heaviest runtime. |
| GTK on Android | Reuse the desktop UI. | Not practical. |

### Core: one library, platform pieces behind small traits

- `TokenSource`: *poppler* on the desktop (today's code, unchanged), *PDFium* on Android
  (`pdfium-render` with the prebuilt `libpdfium.so` for arm64). It gives character boxes, grouped
  into words, converted to the same top-left, points-based coordinates `pdftotext -bbox` produces, so every
  bank profile works unchanged.
- `Ocr`: *tesseract* on the desktop; on Android the app hands page images to ML Kit Text Recognition
  (on-device, offline) and returns tokens to Rust.
- `Http` for exchange rates: `curl` on the desktop, `ureq` with rustls on Android (or a Dart callback).
- Paths are passed in by the host: `Db::open(path)` already takes one; add a `set_data_dir` for the
  statements and attachments folders and the scratch folder.
- Cargo features (`desktop`, `android`) choose the backends, so the desktop build doesn't change.

### FFI: a narrow facade, not 438 functions

A new crate `crates/fingraph-ffi` (bound to Dart by flutter_rust_bridge) exposes **screen-shaped** calls that return plain records, for example
`budget_overview(month)`, `transactions(filter, page)`, `triage_cards()`, `answer_card(...)`,
`subscriptions()`, `outlook()`, `next_steps()`, `add_manual(...)`, `import_pdf(path)`. The view-model
logic that lives in GTK files today (Summary's Next Steps, the budget snapshot) moves into the core, so
desktop and phone share one source of truth. `Db` isn't thread-safe, so the facade owns it on one worker
thread and the Dart side calls it asynchronously.

### Data between devices: complete backups first, a journal second, sync last

**Superseded (10 November 2026)** by [`SERVER-PLAN.md`](SERVER-PLAN.md): a FinGraph server in Docker holds the
ledger and the statements, and the phone and desktop connect to it. Syncthing took too much setup. The text
below is kept as the no-server fallback.

Two devices writing the same SQLite file is how ledgers get corrupted. So, in order:

1. **Phone reads a copy.** The desktop already writes complete backups (ledger + statements + receipts +
   SHA-256 manifest). A new desktop setting, *Copy for the phone*, writes one into a chosen folder after each
   import and on close (keeping a few). That folder is shared by Syncthing between the desktop, the phone
   (Syncthing-Fork) and the home server. The server holds it as an **untrusted** device, so it stores only
   encrypted files it can't read, with **file versioning** on, so a deletion on one device can't erase the
   history. The phone app reads the folder and needs no network permission of its own. Opening a backup file
   shared by hand stays as a fallback. Read-only, nothing to conflict.
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
| **0. Toolchain** | An empty Flutter app calling one Rust function on the test phone, release-signed. | Flutter pointed at the Android SDK, NDK and JDK 17; `flutter_rust_bridge` hello-world over `fingraph-core`; release key; `adb install`; Syncthing on the desktop. | 1 day |
| **1. Portable core** | `fingraph-core` builds for Android; desktop unchanged. | `TokenSource` / `Ocr` / `Http` traits and features; path injection; a **parity test** that reads every sample and demo PDF with both poppler and PDFium and requires the same transactions and reconciliation. | 3–5 days |
| **2. Read-only companion** | Budget, Transactions (search, detail), Subscriptions status, Outlook summary, Next Steps, from a backup. | `fingraph-ffi` facade; move Next Steps and the budget snapshot into the core; desktop *Copy for the phone* setting; five Flutter screens; Syncthing folder picker; app lock. | 2–3 weeks |
| **3. Capture** | Quick-add purchase, receipt camera, *Share PDF to FinGraph* from the bank app; desktop imports the inbox. | Inbox format and desktop importer (with tests); camera plugin; receiving shared files; Material date and amount entry. | 1–2 weeks |
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

## 6. Decisions

| Question | Answer |
|---|---|
| Companion or full import first? | Read-only companion first. |
| How backups reach the phone | Syncthing: desktop, phone (Syncthing-Fork) and the home server as an encrypted, versioned, always-on peer. Not the Pi (SD card storage, no Syncthing there). |
| Platforms | Android now; iPhone possibly later, hence Flutter. |
| Distribution | The owner's phone and GitHub release APKs, signed with the same release key (kept outside the repo) so either installs over the other. |
