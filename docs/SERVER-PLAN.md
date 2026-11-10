# FinGraph Server: plan

Written 10 November 2026, against 0.2.30. A plan, not a promise: estimates are rough and assume one
developer working with an AI agent. It replaces the Syncthing route in [`ANDROID-PLAN.md`](ANDROID-PLAN.md)
(section 3, *Data between devices*), which works but takes far too much setup: three apps, device IDs,
folder IDs, an encryption password typed on every device, and a phone app that didn't keep it.

## 1. What it should be

One Docker container on a machine that's always on (a home server, a NAS, a Pi 4/5). It holds **the
ledger** and **a folder of statement files** that you can mount anywhere. The desktop and the phone point
at it with an address and a pairing code; nothing else to set up.

```
            ┌──────────────── fingraph-server (Docker) ────────────────┐
 Desktop ──▶│  API :8743   ledger (SQLite, one writer)   nightly backups │
 Phone   ──▶│  pairing page          /statements  ◀── mount: NAS, share │
            └──────────────────────────────────────────────────────────┘
```

Without a server, FinGraph keeps working exactly as today: local-first, nothing required.

## 2. Setup the user does

```yaml
# compose.yaml
services:
  fingraph:
    image: ghcr.io/stolly345/fingraph-server:0.3
    ports: ["8743:8743"]
    environment: { PUID: "1000", PGID: "1000", TZ: "Etc/UTC" }
    volumes:
      - ./fingraph-data:/data            # ledger, settings, backups
      - /srv/statements:/statements      # every statement file, readable names
    restart: unless-stopped
```

`docker compose up -d`, open `http://server:8743`, and the page shows a pairing code and a QR code.
On the desktop, *Preferences → Server → Connect* and type the address and code. On the phone, scan the
QR code. That's all: one line per device instead of the Syncthing dance.

## 3. The statements folder

Today statement copies are named by their SHA-256 (`3f9a…c1.pdf`), which is right for the program and
useless to a person browsing a share. On the server, `/statements` follows the owner's own filing: a folder
per year, a folder per month named `01 Jan` … `12 Dec`, and each statement under **its original file name**
from the bank:

```
/statements/
  inbox/                         drop PDFs here (or scan-to-folder from a printer) → imported automatically
  2026/
    09 Sep/
      September 2026 e-statement.pdf
      statement_Main Account_<last4>_September_15_26.pdf
    10 Oct/
      October 2026 e-statement.pdf
      October 2026 e-statement-1.pdf
  needs-attention/               couldn't be read or proved; the reason is in the app's To Review
```

- The month is the statement's **closing date** as the scan reads it, not the day it was downloaded, so a
  September statement downloaded on 3 October still goes in `09 Sep`.
- A different file with the same name in the same month gets ` (2)` added; an identical file (same SHA-256)
  is recognised as already imported and not copied twice.
- The year level is new: the existing `Bank Statements` folder holds one year, and the server will hold many.
- The ledger keeps indexing files by SHA-256, so renaming or moving a file by hand doesn't lose it (the
  server rescans the folder and matches by content). Owner and permissions follow `PUID`/`PGID`, so the
  folder can be an NFS or SMB mount, or a folder Nextcloud or a NAS already backs up.
- Moving to the server files the existing statement copies the same way, so the owner's current
  `Bank Statements` layout carries over.

## 4. Architecture

**The server is the only writer of the ledger.** Two devices writing one SQLite file is how ledgers get
corrupted, and every sync scheme ends up re-solving that. With one writer there's nothing to merge.

- **`crates/fingraph-server`** (new): an HTTP server (`axum` + `tokio`) over `fingraph-core`. It owns the
  `Db` on one worker thread (`Db` isn't thread-safe) and queues requests to it.
- **The core runs unchanged.** The container is Debian slim with `poppler-utils`, `tesseract-ocr` and `curl`,
  so reading PDFs, OCR and exchange rates work exactly as on the desktop. This removes the hardest part of the
  Android plan (Phase 1: PDFium, ML Kit, the parity test) for anyone with a server.
- **API:** JSON over HTTP, screen-shaped as the Android plan already describes (`budget_overview`,
  `transactions`, `triage_cards`, `answer_card`, `import_pdf`, …), versioned (`/api/v1`) so an older app
  gets a clear "update FinGraph" message instead of breaking.
- **Inbox watcher:** files dropped in `/statements/inbox` go through the normal import, with reconciliation
  and review holds, then are filed under their bank and account.
- **Backups:** the existing complete backup format (ledger + statements + receipts + SHA-256 manifest),
  written nightly and before anything drastic, to `/data/backups`, keeping 7 daily, 4 weekly and 12 monthly.
  Restore from the server page.
- **Images:** built by GitHub Actions for `amd64` and `arm64` (so a Pi works) and published on GitHub's
  container registry with the release. Expect about 150 MB, mostly tesseract.

### Security

- **Pairing:** each device gets its own token from a one-time code. The server page lists paired devices
  and can revoke one (a lost phone).
- **Transport:** the server makes its own certificate on first start; the QR code and pairing carry its
  fingerprint, so the apps trust exactly that server (no certificate warnings, no public CA needed).
- **Exposure:** meant for the home network or a VPN (Tailscale, WireGuard). The README says plainly: don't
  publish it to the internet. Login attempts are rate-limited.
- The phone app gains network permission, limited to the paired server (and exchange rates if enabled).

## 5. The desktop

The GTK app calls 134 different `Db` methods from 380 places. Making it a pure remote client in one go is
the biggest piece of work, so it comes in two steps:

1. **Check out / check in (first).** Connected to a server, the desktop downloads the ledger when it opens,
   holding an *edit lease*, works on the local copy as today (fast, works offline), and uploads it on close
   and after each import. While the desktop holds the lease, the server is read-only and the phone shows
   "being edited on alex-X570"; inbox files wait and are imported by the desktop at check-in. A lease
   expires if the desktop vanishes (crash, laptop lid) and the last uploaded copy wins. Reuses the backup
   format; little new code.
2. **Live client (later, only if the lease feels clumsy).** A `Ledger` trait with a local and a remote
   implementation of the 134 methods, generated from one list so they can't drift. Then the desktop and the
   phone edit at the same time.

*Preferences → Server* also offers "Move my ledger to this server" (uploads a complete backup) and "Bring my
ledger back to this computer".

## 6. The phone

With a server, the phone app is a plain Flutter client of the API: no Rust on the phone, no PDF reading, no
Syncthing. It keeps the last answers it got, so it still shows the budget offline ("as of 08:14"). The
read-only screens (Budget, Transactions, Subscriptions, Outlook, Next Steps) come first, then capture
(quick-add, receipt photo, *Share PDF to FinGraph*), which the server applies directly, no inbox file needed.

The Rust bridge from Phase 0 stays in the repo for a later offline mode without a server; it costs nothing
meanwhile.

## 7. Phases

| Phase | Delivers | Main work | Rough size |
|---|---|---|---|
| **S1. Server core** | Container with the ledger, `/statements`, inbox import, nightly backups, status page. | `fingraph-server` crate; Dockerfile; readable filing of statements; PUID/PGID; inbox watcher; backup rotation. Tests for import through the inbox and filing. | 1 week |
| **S2. Pairing and API** | Paired devices, read API, certificate pinning. | Token store; pairing page with QR; `/api/v1` read endpoints (the screen-shaped facade, moved into the core); rate limits. | 1 week |
| **S3. Phone, read-only** | Budget, Transactions, Subscriptions, Outlook, Next Steps from the server; app lock. | Five Flutter screens; QR pairing; offline cache. | 1½–2 weeks |
| **S4. Desktop connected** | Preferences → Server; move ledger to server; check out / check in. | Lease API; upload/download of complete backups; status in the header bar. | 1 week |
| **S5. Phone capture** | Quick-add, receipt photo, share a PDF to FinGraph. | Write endpoints; camera and share-sheet plugins. | 1–1½ weeks |
| **S6. Release** | Image on ghcr.io, APK on GitHub releases, README section with the compose file. | CI for multi-arch images; docs; upgrade test from a previous image. | 3–4 days |

About 6–7 weeks to all of it; S1–S3 (about 3½ weeks) already replaces Syncthing for the phone.

## 8. Risks

- **Lease friction.** If the desktop is left open all day, the phone stays read-only. Mitigation: the desktop
  checks in after each import and when idle for 10 minutes, releasing the lease; it retakes it on the next
  edit. If that's still clumsy, step 5.2 (live client) is the fix.
- **A server that's down.** The desktop still opens its last local copy (read-only, with a banner) and can
  "take the ledger back" if the server is gone for good. Backups on the server's volume are complete and
  restorable by the desktop without the server.
- **Public repo.** No real statements, ledgers or tokens in tests or images. The image contains no data.
- **Support load.** Docker is still setup. For people without a server nothing changes, and the desktop
  remains the whole product.

## 9. Status (0.3.0)

All six phases are built: S1–S5 in 0.3.0, with tests for each (server, client against a real server,
core, phone). S6: the image is built and published by `.github/workflows/server-image.yml` when a
release is published; the APK is built and signed on the owner's machine and attached to the release.

## 10. Decisions (10 November 2026)

| Question | Answer |
|---|---|
| Editing | One device edits at a time: the check-out / check-in lease (5.1) first. |
| Statement names | Year, then month folders `01 Jan` … `12 Dec`, original bank file names inside (section 3). |
| Where it runs | The home server's existing Docker. |
| Syncthing | Once S3 works, the FinGraph Syncthing folder is removed from the desktop, phone and server. |
