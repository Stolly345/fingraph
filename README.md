# FinGraph

A local-first ledger for Cayman bank statements, in Rust with GTK 4 and libadwaita. Drop PDF
statements in; a 5-pass scanner reads them and proves each one against its own balances; Smart
Triage learns your merchants; the dashboard shows where the money goes, in KYD or USD.

Nothing leaves the computer. The ledger is `~/.local/share/fingraph/ledger.db` (SQLite, WAL).

## Layout

| Crate | What |
|---|---|
| `crates/fingraph-core` | No GUI. Scanner (`parser/`), ledger (`db.rs`), rules & triage (`rules.rs`), analytics (`insights.rs`), CSV/QIF/OFX/QBO (`export/`), simulated statements (`demo.rs`) |
| `crates/fingraph` | The app: views, Cairo charts (`charts.rs`), onboarding + Smart Triage (`wizard.rs`), Statement Inspector |

Money is `rust_decimal::Decimal` end to end, stored as the exact string plus integer cents next to
its currency. The KYD/USD peg (1.00 KYD = 1.20 USD) is the default rate; an FX line printed on the
statement (`USD 145.00 @ 0.84`, `(59.14 KYD)`) always wins for that transaction.

## The scanner

Words and their positions come from `pdftotext -bbox` (poppler-utils).

1. **Fingerprint & metadata** — score every bank profile (page-1 matches count triple), find the
   period (locks years, Dec→Jan rollover, DD/MM vs MM/DD), account, type, currency and the summary box.
2. **Column geometry** — header words name the columns (multi-line headers merged, margin lettering
   skipped), then each money column is locked to where its amounts' right edges actually line up.
   Rotated margin text and noise lines are dropped.
3. **Transactions** — rows by locked columns; RBC-style rows inherit the date above; description
   lines are stitched into the memo, including vertically centred multi-line card descriptions;
   dual-currency cards and credit-union statements split into one section per account.
4. **The math** — `Bal[i-1] + Amt[i] = Bal[i]` for every stretch between printed balances (a single
   row whose flipped sign fixes the stretch is healed), and `Opening + Σ = Closing`.
5. **Re-scan** — a failing stretch names the two rows the missing one sits between; skipped lines
   there are re-read leniently (drifted amounts, page-break split rows). Whatever is left is shown in
   the Inspector with a one-click "Convert".

Built-in profiles: Butterfield, Scotiabank, Cayman National, RBC, CICSA Credit Union.

## Run

```sh
sudo apt install poppler-utils libgtk-4-dev libadwaita-1-dev
cargo run -p fingraph --release
cargo run -p fingraph-core --example scan -- statement.pdf   # scanner report in the terminal
cargo deb -p fingraph                                          # .deb with desktop entry and icon
```

Main menu → **Load Demo Data** imports a simulated year (Personal + a property business, all five
banks, KYD and USD) so every chart, the HBO/CUC triage cards and the Inspector have something to show.

## Tests

```sh
cargo test -p fingraph-core                      # 5 bank layouts, rollover, inheritance, healing, demo year
cargo test -p fingraph-core -- --ignored real    # also scans your own statements in ~/.local/share/fingraph/documents
```
