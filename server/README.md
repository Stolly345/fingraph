# FinGraph Server

Holds the FinGraph ledger and your statement files on a machine that's always on, so the desktop app
and the phone app share them. See the main [README](../README.md#fingraph-server-and-the-phone-app-optional)
for what it does and [`docs/SERVER-PLAN.md`](../docs/SERVER-PLAN.md) for how it's built.

## Run it

```sh
mkdir fingraph && cd fingraph
curl -O https://raw.githubusercontent.com/Stolly345/fingraph/main/server/compose.yaml
# edit PUID/PGID (id -u, id -g), TZ and FINGRAPH_ADDRESS
docker compose up -d
docker compose logs fingraph      # the first pairing code and QR code
```

## Commands

Run inside the container (`docker exec fingraph …`):

| Command | Does |
|---|---|
| `fingraph-server pair` | A one-time pairing code (15 minutes, 5 tries), the fingerprint to compare and, with `FINGRAPH_ADDRESS` set, a QR code for the phone |
| `fingraph-server devices` | Paired devices, when each was last seen, and which one is editing |
| `fingraph-server unpair ID` | Unpairs a device (a lost phone): its access ends at once and a lease it held is released |
| `fingraph-server backup` | A complete backup now |

## Settings

| Variable | Default | |
|---|---|---|
| `PUID`, `PGID` | `1000` | Who owns the files the server writes (your user, so you can read the statements folder) |
| `TZ` | `Etc/UTC` | Time zone, for "today" and the backup hour |
| `FINGRAPH_ADDRESS` | | How phones and computers reach the server (`192.168.1.20:8743`); used for the QR code |
| `FINGRAPH_BACKUP_HOUR` | `3` | Hour of the nightly backup |
| `FINGRAPH_INBOX_SECONDS` | `15` | How often the inbox is looked at. Polling, so NFS and SMB mounts work |
| `FINGRAPH_SETTLE_SECONDS` | `10` | A file younger than this may still be arriving and waits for the next look |
| `FINGRAPH_LISTEN` | `0.0.0.0:8743` | Where it listens inside the container |

## Folders

```
/data                    (keep it private; the server sets it to 700)
  ledger.db              the ledger
  server.db              paired devices, the edit lease, which statements are filed
  statements/            the ledger's own copy of each statement, named by SHA-256
  attachments/           receipts
  backups/               fingraph-YYYY-MM-DD.tar, complete and checked; restores in the desktop
  captures/              purchases and receipts from the phone waiting for a computer to finish editing
  tls/                   the server's certificate and key (the apps pin this certificate)
/statements              yours: mount a NAS share, Nextcloud folder, anything
  inbox/                 drop PDFs here
  2026/10 Oct/…          every statement, under its closing month, with the bank's file name
  needs-attention/       files that need a person, each with a .txt saying why
```

Renaming or moving files in `/statements` by hand is safe: the ledger keeps its own copy of each one.
Deleting `/data/tls` makes a new certificate, and every device then has to pair again.

## Upgrading

`docker compose pull && docker compose up -d`. The ledger is brought up to the new version on start,
the same way the desktop does it; take a backup first (`docker exec fingraph fingraph-server backup`)
if you want to be able to go back to the older image.

## Build it yourself

From the repository root: `docker build -f server/Dockerfile -t fingraph-server .`
Or without Docker: `cargo run -p fingraph-server --release` (needs `poppler-utils`, and `tesseract-ocr`
for picture-only statements), with `FINGRAPH_DATA` and `FINGRAPH_STATEMENTS` pointing at two folders.

`cargo run -p fingraph-client --example fingraph-remote` is a small command-line client for trying a
server by hand (pair, status, views, sending a statement).
