# FinGraph

A local-first ledger for Cayman bank statements, in Rust with GTK 4 and libadwaita. Drop PDF
statements in; a 5-pass scanner reads them and proves each one against its own balances; a Cayman
business directory and Smart Triage categorise them; the dashboard shows where the money goes, in
KYD or USD, across personal and business workspaces.

Nothing leaves the computer. Everything lives in `~/.local/share/fingraph/` (ledger, kept statement
PDFs, backups). Full product and system reference: [`docs/PRD.md`](docs/PRD.md).

## What it does

- **Reads statements** from Butterfield, Scotiabank, Cayman National, RBC and the CICSA Credit
  Union, and reconciles them to the cent (running balances and opening + transactions = closing).
  What it can't read is shown in the Statement Inspector with a one-click fix.
- **Categorises** with your own rules first, then internal transfers and credit card payments,
  then a directory of 571 Cayman businesses and online services, then keyword guesses
  (MARKET → groceries, DIGITAL → subscriptions). Whatever's left goes to **To Review**: one card per
  merchant, every charge listed, answered once and applied to past and future statements.
- **Keeps money apart**: workspaces (Personal, Business, Rental), a consolidated view, a
  business-expense flag, split charges (the rental's share of one receipt), and "Business owes you"
  cards that track personal money spent on the business until it's paid back.
- **Budget**: a monthly amount per category, filled from what you usually spend (median of the last
  six months), with this month against it, what's over, and where the month is heading. A setup
  wizard takes your pay cycle, gross pay and deductions (pension 5%, health insurance), other income,
  loan payments and a savings goal, does the arithmetic (and solves how much flexible spending must
  shrink to reach the goal), and projects savings 12 months ahead on budget and on trend. Alerts when
  a category is over or 85% used early in the month (sidebar count + Overview card).
- **Rules** can be limited to an amount (*only when CI$ 125.00*) or match a reference number.
- **Receipts**: drop a PDF or photo on a transaction; export the P&L with every receipt for your
  accountant.
- **Statement coverage**: an account × month grid shows verified, missing and broken months.
- **Shows** an overview with six interactive charts, What Changed (price rises, bill spikes,
  possible double charges, bank fees, category drift, P&L, annual renewals; bills compared with the
  same month last year), subscriptions with their real yearly cost, an overview per account, a
  **Combined Statement** (all or chosen accounts, month by month with a combined running balance;
  click a line to edit it), and
  projects (tags) with total landed cost including freight and customs duty.
- **Exports** CSV, QIF, OFX 1.0.2, QuickBooks QBO, a P&L, and a P&L with receipts.

Money is `rust_decimal::Decimal` end to end, stored as the exact string plus integer cents next to
its currency. The KYD/USD peg (1 KYD = 1.20 USD) is the default rate; a rate printed on the statement
wins for that transaction.

## Layout

| Path | What |
|---|---|
| `crates/fingraph-core` | No GUI: scanner (`parser/`), ledger (`db.rs`), rules & triage (`rules.rs`), directory (`merchants.rs`, `data/merchants.tsv`), splits/reimbursements/attachments/coverage (`workflows.rs`), insights, exports, preferences, demo statements |
| `crates/fingraph` | The app: one file per page (`dashboard`, `review`, `changes`, `transactions`, `account`, `statements`, `rules`, `profiles`, `export`, `preferences`, `wizard`, `split`, `owed`, `statement`, `txedit`, `directory`, `budget`), Cairo charts in `charts.rs` |
| `packaging/` | Desktop entry (PDF "Open With") and icon |
| `docs/PRD.md` | Product and system reference |

## Install

```sh
sudo apt install poppler-utils libgtk-4-dev libadwaita-1-dev
cargo deb -p fingraph
sudo apt install ./target/debian/fingraph_0.1.0-1_amd64.deb
```

Or run from source: `cargo run -p fingraph --release`. Main menu → **Load Demo Data** imports a
simulated year (Personal + a property business, all five banks, KYD and USD) to explore everything.

## Using it

1. Drop statements on **Statements** (or set a watched folder in Preferences → Import).
2. Answer **To Review**. Each answer becomes a rule.
3. Check **Overview** and **What Changed**; click any account in the sidebar for its breakdown.

Shortcuts: Ctrl+1 … Ctrl+9 for the main pages, Ctrl+F search, Ctrl+O import, Ctrl+, preferences.

## Adding businesses

`crates/fingraph-core/data/merchants.tsv`, one line each:

```
KEYWORDS<TAB>Name<TAB>Category[<TAB>fallback]
HOME GAS|HOMEGAS	Home Gas	Gas
```

Keywords match whole words (punctuation and apostrophes ignored, a trailing S allowed); the earliest
match in the statement text wins, then the longest. Never add a generic word on its own. `fallback`
marks a payment platform (Google, Apple) that gives way to the service it billed. Rebuild after
editing. Browse it in the app under **Setup → Business Directory** (search, how often each
business appears in your statements, keyword guesses, and **Override…** to make your own rule). For a one-off, answering in To Review (or a rule) is quicker and always wins.

## Terminal tools

```sh
cargo run -p fingraph-core --example scan -- statement.pdf          # scanner report for one PDF
cargo run -p fingraph-core --example ledger -- reapply              # re-run rules, card payments, directory
cargo run -p fingraph-core --example ledger -- rescan               # re-read every kept statement
cargo run -p fingraph-core --example ledger -- cards                # recognise credit card payments
cargo run -p fingraph-core --example ledger -- reset                # forget learned rules and categories
```

Close the app first; back up from Preferences → Data (or it backs up automatically before Reset and
Rescan when run from the app).

## Tests

```sh
cargo test --workspace                            # 47 tests: bank layouts, healing, ledger, rules, directory, insights, exports
cargo test -p fingraph-core -- --ignored real     # also scans your statements in ~/.local/share/fingraph/documents
cargo clippy --workspace --all-targets
```
