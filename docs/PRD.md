# FinGraph — PRD / System Reference

**Repo:** `~/Projects/fingraph-rs` (Rust 2024, GTK 4 + libadwaita) · **Package:** `fingraph` 0.1.0 `.deb`
**Owner:** alex · **Doc written:** 2026-10-27 (as built) · **Status:** in daily personal use; not published

> Single source of truth for what FinGraph is, how it's built, and why it behaves the way it does.
> It replaces the Python/Flet prototype in `~/Projects/FinGraph` (left untouched; its
> `~/.config/fingraph/data.db` was not migrated). The original build brief is section 2; everything
> after it records what was actually built, including the changes requested while using it.

---

## 1. Purpose & Goals

A local-first ledger for people banking in the Cayman Islands. Drop in PDF bank and card
statements; FinGraph reads them, **proves each one against its own balances**, learns what your
merchants are, and shows where the money goes, in KYD or USD, across personal and business
workspaces.

**Goals**
- Extract every transaction from Butterfield, Scotiabank, Cayman National, RBC and CICSA Credit
  Union statements, and know when it hasn't (reconciliation to the cent, not "looks right").
- Categorise with as little effort as possible: a Cayman business directory, keyword guesses, and
  rules learned from one answer per merchant, applied to the past and the future.
- Keep personal and business money apart (workspaces, business-expense flag) while allowing a
  consolidated view.
- Never treat money moving between your own accounts (card payments, savings sweeps) as spending.
- Exact money: `rust_decimal`, never floats; amounts stored as exact strings plus integer cents.

**Non-goals / constraints**
- 100% local and private: no cloud APIs, telemetry or LLMs. Data in `~/.local/share/fingraph/`.
- No bank connections or live balances: balances shown are the last ones printed on a statement.
- No OCR: image-only (scanned) PDFs are rejected with a clear message.
- Not multi-user, not a budgeting app (no budgets/envelopes), not double-entry accounting.

---

## 2. Original brief (summary)

Two-crate workspace in Magpie's style: `fingraph-core` (no GUI: scanner, SQLite, decimal money,
KYD/USD, rules, transfer matching, insights, CSV/QIF/OFX/QBO) and `fingraph` (GTK app with Cairo
charts, triage wizard, bank-profile inspector). Five built-in Cayman bank profiles. A 5-pass scanner
(fingerprint → column geometry → extraction & stitching → running-balance and summary math →
self-healing re-scan with an inspector for leftovers). Smart Triage grouping merchants into
conversational cards. KPI ribbon, six interactive charts and "What changed?" anomaly cards.
First-run walkthrough and demo data. Clippy and release build clean.

All of it was built; sections 4–9 describe how.

---

## 3. Users & how they use it

One person (or household) managing their own money, sometimes with a small business or rental
property. Three ways of using the app shaped the layout:

1. **Monthly routine** — download statements, drop them in (or let the watched folder pick them
   up), answer To Review, glance at Overview and What Changed.
2. **Looking something up** — "what did I pay Adriana?", "how much is on the Visa?": Transactions
   search, an account's overview, a chart click-through.
3. **Setup** (rare) — workspaces, rules, categories, bank profiles, preferences.

---

## 4. Architecture

| Crate / file | Responsibility |
|---|---|
| `fingraph-core/src/parser/` | 5-pass scanner: `multipass.rs` (passes 1–3), `reconcile.rs` (4–5), `profiles.rs` (bank profiles, fingerprinting), `dates.rs` |
| `fingraph-core/src/db.rs` | SQLite ledger (WAL), migrations, import, transfers, card payments, rules, backups |
| `fingraph-core/src/rules.rs` | Merchant stems, learned rules, truncation-tolerant matching, Smart Triage grouping |
| `fingraph-core/src/merchants.rs` + `data/merchants.tsv` | Merchant directory (571 entries) and keyword rules |
| `fingraph-core/src/insights.rs` | KPIs, chart series, subscriptions, "What changed?" cards, balances |
| `fingraph-core/src/export/` | CSV, QIF, OFX 1.0.2 SGML, QBO, P&L CSV |
| `fingraph-core/src/prefs.rs` | Preferences (stored in the ledger, so they travel with backups) |
| `fingraph-core/src/demo.rs` | Simulated statements for all five banks (tests + Load Demo Data) |
| `fingraph/src/*.rs` | One file per page/dialog; `charts.rs` is the Cairo chart widget |

Text and positions come from poppler's `pdftotext -bbox` (no font metadata; geometry does the work).
`examples/scan.rs` prints a scanner report; `examples/ledger.rs` runs reset / rescan / reapply / cards
from a terminal.

---

## 5. Data model (schema v3)

`entities` (workspaces: Personal / Business / RentalProperty, base currency) · `bank_profiles` ·
`accounts` (bank, masked number, type, currency, OFX ids, `hidden`) · `fx_rates` (peg 1 KYD = 1.20 USD)
· `documents` (one per statement *section*: period, opening/closing, totals, reconciliation verdict,
confidence, full scan diagnostics JSON, keyed by file SHA-256 + section) · `transactions` (posted
and effective dates, raw description, clean payee, amount + cents, currency, amount in the workspace
currency, reported running balance, category, tag, memo, FITID, recurring/subscription flags,
`transfer_peer_tx_id`, `business_entity_id`, review status) · `categories` (108, two levels, kinds
Income / Expense / Transfer) · `learned_rules` (kind, pattern, payee, category, subscription,
transfer, cadence, workspace scope, business workspace) · `settings` (incl. the preferences JSON).

- **FITID** = SHA-256 of account, date, amount, normalised description and same-day sequence:
  re-importing a statement or an overlapping one never duplicates.
- **Workspace amount**: a rate printed on the statement (`USD 145.00 @ 0.84`, `(59.14 KYD)`) beats
  the configured rate (switchable).
- Migrations: v1 base, v2 business-expense columns, v3 hidden accounts. Missing categories are
  seeded into older ledgers on open; nothing is renamed or removed.

---

## 6. Statement scanning

1. **Fingerprint & metadata** — every profile scored (page-1 matches ×3); period (locks years,
   Dec→Jan rollover, DD/MM vs MM/DD); account last four; type; currency; summary box, including
   dual KYD/USD columns.
2. **Column geometry** — header words (multi-line headers merged, margin lettering skipped), then
   money columns locked to where amounts' right edges line up; rotated margin text and noise dropped.
   Tables end at the closing balance.
3. **Transactions** — rows by column; RBC-style rows inherit the date above; description lines
   stitched into memos, including vertically centred multi-line card descriptions; one section per
   account (dual-currency cards, credit-union share + loan).
4. **The math** — `Bal[i-1] + Amt[i] = Bal[i]` for every stretch between printed balances (a single
   row whose flipped sign fixes a stretch is healed), and opening + Σ = closing.
5. **Re-scan** — the failing stretch names the two rows the missing one sits between; skipped lines
   there are re-read leniently (drifted amounts, page-break split rows). Leftovers appear in the
   Statement Inspector with one-click **Convert**.

**Verified on real data:** all 12 of the owner's Scotiabank statements (16 sections: chequing ••5311,
Amex Platinum Dual ••3575 KYD+USD, Mastercard ••2197) reconcile to the cent. The other four banks
are verified against simulated layouts only (see §13).

Imported PDFs are kept (by SHA-256) in `~/.local/share/fingraph/statements/` so **Rescan** can
re-read everything; a rescan keeps hand edits (categories, payees, tags, business flags,
inspector-converted lines) matched by FITID.

---

## 7. Categorisation pipeline

Applied on import, in this order; earlier steps win.

1. **Your rules** (learned from To Review or "Remember" toasts; workspace rules beat global ones).
2. **Transfers** between your accounts: opposite amounts, same currency exactly or KYD↔USD within 2%
   of the peg, ≤ 5 days apart, transfer wording or the other account named. Never spending.
3. **Credit card payments**: a bank payment naming one of your cards (last four, or a full number
   ending in them) or saying it pays a card, and payments received on a card account → Transfers ›
   Credit Card Payments, payee "Payment to <card>". Works without the card's statement.
4. **Merchant directory** (`data/merchants.tsv`, 571 entries: Cayman supermarkets, liquor, pharmacies,
   hardware, ~150 restaurants, insurers, utilities incl. Home Gas / Clean Gas propane and Logic /
   Flow / C3 / Digicel, fuel, government fees, auto, health, schools, hotels, airlines; online
   retailers, VPNs, email, every Google service, Apple, Microsoft, streaming, AI, software, games).
   Whole-word matching; earliest match wins (merchant before billing descriptor); `fallback` entries
   (GOOGLE, APPLE.COM/BILL) lose to the service they billed. Amazon store → Online Shopping, Amazon
   Digital/Prime/Kindle/Audible → Subscriptions & Digital. Filed automatically, marked for review.
5. **Keyword guesses** (MARKET/MKT → Groceries, DIGITAL/DGTL → Subscriptions & Digital, INSURANCE*,
   PHARMACY, AUTO, SALON…): suggested in To Review; filed automatically only if switched on.
   MKTPL / MARKETPLACE / MARKETING are never groceries.

A transaction you set to Uncategorized by hand is left alone by steps 2–5. Checked against the
owner's hand-categorised ledger: the directory agreed 360 times and differed 37 (mostly
business/property choices, which the owner's rules keep).

**Smart Triage / To Review** groups uncategorised charges by merchant stem, treating truncated
spellings as one merchant (`DISCOVERYDI HELP` / `HEL` / `HE`) and well-known services printed
differently as one (`HBO MAX` / `PAYPAL *HBOMAX`). Each card lists every charge with date, account
and the statement's wording; asks *subscription?*, *internal transfer?* or *which category?*; has a
clean-payee field, rule scope (this workspace / all) and a **Business expense** switch. Answers become
rules applied retroactively.

**Business expenses** paid from a personal account keep their account but count in the chosen
business workspace's P&L (`business_entity_id`); per transaction, in bulk, from triage, or as a rule.

---

## 8. Insights

- **Overview**: Net cash flow (vs prior period), savings/operating margin, active subscription burn,
  reconciliation & triage status; charts: monthly income/expenses + net line, spending by category
  (donut → merchant drill-down), cumulative daily spend vs the usual band, top-merchant Pareto,
  balance history, workspace/tag P&L. Every chart has tooltips and click-through to transactions.
- **What Changed**: one card per kind with an expandable list — price creep (last rise within 90
  days), bill spikes (> 20% over the bill's own 6-month average, steady bills only), possible double
  charges (same account, merchant and amount within 3 days; fees and subscriptions never; a run is one
  entry), bank fees & interest (by bank and fee type), category drift, YTD P&L by workspace/tag.
- **Subscriptions**: per merchant, cost per cycle, yearly cost, last charge, price rises, lapsed.
  Billing cycle from the charges' rhythm; a lone charge is guessed **yearly** (a monthly plan
  corrects itself on its next charge); a picker (weekly … yearly) overrides any guess.
- **Account overview**: spent, received, transfers, latest verified balance, by category, top
  merchants, month by month, balance history — in the account's own currency.

Thresholds, chart months, hidden cards and categories left out of spending are preferences.

---

## 9. Interface

**Sidebar** (`AdwNavigationSplitView`): workspace switcher (one workspace or Consolidated across a
chosen set) at the top; Overview, To Review *(count)*, What Changed *(count)*, Transactions,
Subscriptions *(monthly cost)*, Statements *(⚠ when one doesn't balance)*, Reports & Export
(Ctrl+1–7); **Accounts** — each with its last verified balance (owed in red), opening its overview;
**Setup** — Rules & Categories, Bank Profiles, Preferences (Ctrl+,). The header shows only the
controls a page uses (date range, KYD | USD). Reopens on the last page.

**Pages**: Transactions (virtualised grid, search Ctrl+F, account/category filters, inline category,
payee, business and tag edits with "Remember" toasts, bulk categorise/tag/business/subscription/
unlink/delete) · Statements (drop zone, bank badge, reconciliation badge, Scan Pass Report for every
pass, Convert for skipped lines) · Reports & Export (CSV, QIF, OFX, QBO per account, P&L CSV, range
summary) · Rules & Categories (edit, re-apply, forget, add category) · Bank Profiles (edit, test a
PDF, new profile from a sample's detected columns).

**Preferences** (7 pages, saved as they change): General (start workspace, date range, currency,
light/dark) · Money (rate, statement rates win, codes vs symbols, brackets) · Import (statement
copies folder, watched folder for new statements, triage after import, double-charge window) ·
Triage (shortened names, business directory, keyword auto-filing, subscription questions, rule scope,
rescan keeps edits; re-apply / rescan / reset) · Dashboard (cards, thresholds, months, excluded
categories) · Workspaces & accounts (rename, currency, add, delete; rename, move, hide, OFX ids) ·
Data (ledger location, automatic backups before risky changes — last ten kept, back up / restore,
export everything, erase all).

**First run**: walkthrough (workspaces → banks & statements with live detection → triage) and
**Load Demo Data** (a year of simulated Personal + property-business statements across all five
banks, one deliberately unreadable line to show the inspector).

---

## 10. Packaging & operations

- `cargo deb -p fingraph` → `target/debian/fingraph_0.1.0-1_amd64.deb` (binary, desktop entry with
  PDF "Open With", icon; depends on poppler-utils, GTK 4, libadwaita).
- Install: `sudo apt install ~/Projects/fingraph-rs/target/debian/fingraph_0.1.0-1_amd64.deb`.
- Data: `~/.local/share/fingraph/ledger.db` (+ `statements/`, `backups/`). Hand backups made during
  development sit beside the ledger as `ledger-backup-2026-10-27-*.db`.
- Terminal: `cargo run -p fingraph-core --example ledger -- reset|rescan|reapply|cards`.
- Debug builds only: `FINGRAPH_SHOT=out.png` (with `FINGRAPH_DB`, `FINGRAPH_DEMO`, `FINGRAPH_VIEW`,
  `FINGRAPH_DIALOG`, `FINGRAPH_PAGE`, `FINGRAPH_SHOT_STEPS`, `FINGRAPH_SCROLL`) renders the window to
  a PNG for visual checks without a person at the screen.

---

## 11. Quality

- `cargo test --workspace`: 36 tests — five bank layouts, rollover, date inheritance, sign healing,
  both Pass-5 recoveries, a whole demo year, ledger (dedup, transfers, card payments, business
  expenses, backups/restore/erase, rescan keeping edits), rules and truncation matching, directory
  and keywords, insights, subscriptions, exports. Plus `-- --ignored real` re-scanning the owner's
  statements.
- `cargo clippy --workspace --all-targets` clean; `cargo build --release` clean.
- UI checked page by page with the screenshot hooks against the demo and a copy of the real ledger.

---

## 12. Decisions log (changes made while in use)

| Request | Decision |
|---|---|
| Triage showed "$500 to Adriana" | Totals were sums of several payments; cards say "4 payments of CI$ 125.00 each" and list each charge with its date |
| Reset triage / relearn | Reset Learning & Re-triage; Rescan All Statements; PDFs kept for rescans |
| Business expense toggle | `business_entity_id`: the expense counts in the business, the account stays |
| More categories, A–Z, Family Expense | 29 top-level categories, sorted A–Z everywhere; Add a Category |
| Discovery charged 3 ways | Truncation-tolerant merchant matching for groups and rules |
| Service charges flooding What Changed | Fees never double charges; runs collapse; one expandable card per kind |
| Preferences | Seven pages (§9) |
| Sidebar redesign | Arranged around the monthly routine / look-up / setup split (§3, §9) |
| Cayman business database, keywords | `merchants.tsv` + keyword rules (§7) |
| Amazon isn't groceries | Store → Online Shopping, digital → Subscriptions; MKTPL never MARKET |
| Online retailers, VPNs, email, Google | Added; payment platforms as fallbacks |
| Card payments | Transfers › Credit Card Payments, recognised by card number or wording |
| ExpressVPN at CI$ 1,215/yr | Lone charge guessed yearly; billing-cycle picker |
| Per-account breakdown | Account overview page from the sidebar |

---

## 13. Outstanding / known limits

- **Four of five banks unverified on real PDFs** (Butterfield, CNB, RBC, CICSA): first real
  statement may need profile tuning (Bank Profiles → Test a PDF).
- **Directory accuracy**: most entries came from research agents with little web access; generic
  keys were removed, but individual entries may be wrong or closed. Harmless if absent from your
  statements; fix by editing `data/merchants.tsv` (rebuild needed) or with a rule.
- **No statement-coverage awareness** yet: yearly totals and "vs prior period" don't know which
  months have statements; gaps between statements aren't flagged (proposed, not built).
- No OCR for scanned PDFs; no font metadata from the PDF.
- The directory isn't browsable in the app; additions are rules or file edits.
- The Python prototype's ledger was never migrated (statements re-imported instead).
- Not committed to a remote; no CI.
