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

## 5. Data model (schema v5)

`entities` (workspaces: Personal / Business / RentalProperty, base currency) · `bank_profiles` ·
`accounts` (bank, masked number, type, currency, OFX ids, `hidden`) · `fx_rates` (peg 1 KYD = 1.20 USD)
· `documents` (one per statement *section*: period, opening/closing, totals, reconciliation verdict,
confidence, full scan diagnostics JSON, keyed by file SHA-256 + section) · `transactions` (posted
and effective dates, raw description, clean payee, amount + cents, currency, amount in the workspace
currency, reported running balance, category, tag, memo, FITID, recurring/subscription flags,
`transfer_peer_tx_id`, `business_entity_id`, review status) · `categories` (108, two levels, kinds
Income / Expense / Transfer) · `learned_rules` (kind, pattern, payee, category, subscription,
transfer, cadence, workspace scope, business workspace, optional amount range) · `splits` (lines of
a split charge: amount, category, workspace, tag, memo) · `reimbursements` (which business charge or
split line was paid back by which incoming transaction) · `attachments` (receipt files, keyed by
account + FITID so they survive rescans) · `budgets` (view scope = sorted workspace ids,
top-level category, monthly amount + currency) · `settings` (incl. the preferences JSON).

- **FITID** = SHA-256 of account, date, amount, normalised description and same-day sequence:
  re-importing a statement or an overlapping one never duplicates.
- **Workspace amount**: a rate printed on the statement (`USD 145.00 @ 0.84`, `(59.14 KYD)`) beats
  the configured rate (switchable).
- **Splits**: the lines must add up to the bank's amount; the statement row keeps its FITID and running
  balance (reconciliation is unchanged), while category totals, P&L, projects and "owes you" read the
  lines. Transfers can't be split. Splits, reimbursements and attachments follow the FITID through rescans.
- **Attachments** are copied (named by content hash) into `~/.local/share/fingraph/attachments/`.
- Migrations: v1 base, v2 business-expense columns, v3 hidden accounts, v4 amount rules, splits,
  reimbursements, attachments; v5 budgets. Missing categories are
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
   A rule can be limited to an amount or range (*Adriana only when CI$ 125.00*; other amounts go to
   review) and can match a standing order's reference number. To Review offers "Only for CI$ 125.00
   payments" when every charge is the same amount; Transactions → **Rule…** prefills from one row.
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
A **split** sends part of one charge to the business (e.g. the rental's share of a hardware receipt).
The Overview shows **"<Business> owes you"** for business charges paid personally and not yet repaid;
**Record Repayment** links them to the incoming transfer that paid them back (undo exists in the core
only).

---

## 8. Insights

- **Overview**: Net cash flow (vs prior period), savings/operating margin, active subscription burn,
  reconciliation & triage status; charts: monthly income/expenses + net line, spending by category
  (donut → merchant drill-down), cumulative daily spend vs the usual band, top-merchant Pareto,
  balance history, workspace/tag P&L. Every chart has tooltips and click-through to transactions.
  "vs prior period" is only shown when statements cover ≥ 80% of the prior period's account-months.
- **What Changed**: one card per kind with an expandable list — price creep (last rise within 90
  days), bill spikes (> 20% over the bill's own 6-month average, steady bills only), possible double
  charges (same account, merchant and amount within 3 days; fees and subscriptions never; a run is one
  entry), bank fees & interest (by bank and fee type), category drift, YTD P&L by workspace/tag,
  **annual renewals** (a charge ~a year after the same merchant's last one). Bill spikes also compare
  with **the same month last year** when it exists, so a CUC summer bill isn't flagged against winter.
- **Statement coverage** (Statements page): an account × month grid — ● verified, ◐ discrepancy,
  ○ missing, · before the account's first statement — plus breaks where one statement's closing
  balance isn't the next one's opening, or dates leave a gap of more than 3 days. Duplicate and
  overlapping statements are skipped. Counted in the sidebar's Statements badge.
- **Projects & landed cost** (Reports): every tag as a project across accounts, with category
  breakdown; for an online order it suggests the freight-forwarder (MyUS, Tropical, Aeropost,
  Caribex, Hyde…) and customs charges that followed it — **Add N freight & duty** tags them.
- **Subscriptions**: per merchant, cost per cycle, yearly cost, last charge, price rises, lapsed.
  Billing cycle from the charges' rhythm; a lone charge is guessed **yearly** (a monthly plan
  corrects itself on its next charge); a picker (weekly … yearly) overrides any guess.
- **Budget**: per top-level expense category, a monthly amount kept per view (Personal, the
  business and a consolidated set each have their own). "Usually" = median of the last 6 whole
  months, months without spending counted as zero, rounded up to 10; **Fill From My Spending** sets
  it for every category without a budget, **Use** per row. Shows spent / budget with a bar (red when
  over), last month, totals, spending outside budgeted categories, uncategorised spending (→ To
  Review) and the month's projected total at the current pace. Previous months browsable.
  - **Alerts**: *over* (spent > budget) or *near* (≥ 85% used while < 85% of the month has gone);
    sidebar badge ("2 over" in red, else "1 near"), an Overview card, amber/red bars.
  - **Setup wizard** (Set Up Budget… / Edit Plan…), 4 steps: pay (cycle weekly / fortnightly /
    twice a month / monthly, next payday, gross, deductions as amount or % of gross; prefilled from a
    detected salary — same payer ≥ 3× in 120 days at a steady rhythm and amount, payroll wording
    preferred), other income and loan payments (median of 6 months of Loan Payments), category
    budgets (fixed bills vs flexible), savings goal (amount or % of take-home). Shows take-home −
    spending − loans = savings and, when short, solves take-home − loans − fixed − k·flexible = goal
    for k and offers to trim flexible categories (to the nearest 5). Plan saved per view as JSON
    (`budget_plan:<scope>` setting).
  - **Your Plan** card + **12-month savings projection**: on budget (constant monthly savings) vs on
    trend (each category's 6-month least-squares line carried forward, floored at 0) vs goal; next
    three paydays (monthly pay keeps its day, clamped to month end). Rows note a trend of ≥ 5%
    (min CI$ 5) a month.
- **Combined Statement**: every transaction of the ticked accounts (default all in the workspace)
  in date order, a heading per month (in / out / net, transfers not counted, closing combined
  balance), the combined balance after each line (printed balances, else carried from the
  statement's opening; owed amounts negative; shown once every ticked account has one), a
  jump-to-month list. Clicking a line opens the transaction editor.
- **Account overview**: spent, received, transfers, latest verified balance, by category, top
  merchants, month by month, balance history — in the account's own currency.

Thresholds, chart months, hidden cards and categories left out of spending are preferences.

---

## 9. Interface

**Sidebar** (`AdwNavigationSplitView`): workspace switcher (one workspace or Consolidated across a
chosen set) at the top; Overview, To Review *(count)*, What Changed *(count)*, Transactions,
Combined Statement, Budget, Subscriptions *(monthly cost)*, Statements *(⚠ when one doesn't balance)*, Reports & Export
(Ctrl+1–7; Combined Statement Ctrl+8, Budget Ctrl+9); **Accounts** — each with its last verified balance (owed in red), opening its overview;
**Setup** — Rules & Categories, Business Directory, Bank Profiles, Preferences (Ctrl+,). The header shows only the
controls a page uses (date range, KYD | USD). Reopens on the last page.

**Pages**: Transactions (virtualised grid, search Ctrl+F, account/category filters, inline category,
payee, business and tag edits with "Remember" toasts, bulk categorise/tag/business/subscription/
unlink/delete, ✂ split, 📎 attach — or drop a file on a row — and Rule…; a paperclip opens the
receipts) · Statements (drop zone, bank badge, reconciliation badge, Scan Pass Report for every
pass, Convert for skipped lines) · Reports & Export (CSV, QIF, OFX, QBO per account, P&L CSV, **P&L with receipts** — a folder with
the P&L and every attached receipt named `date payee amount` — and Projects & Landed Cost) · Rules & Categories (edit, re-apply, forget, add category) · Bank Profiles (edit, test a
PDF, new profile from a sample's detected columns).

**Transaction editor** (from the Combined Statement and the "owes you" dialog's ✎): payee,
category, which workspace it counts in, tag, memo, subscription, Split…, receipts, Show in
Transactions; changes apply as made.

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
  `FINGRAPH_DIALOG` (`triage|onboarding|report|preferences|split:<id>|owed:<id>`), `FINGRAPH_PAGE`, `FINGRAPH_SHOT_STEPS`, `FINGRAPH_SCROLL`) renders the window to
  a PNG for visual checks without a person at the screen.

---

## 11. Quality

- `cargo test --workspace`: 47 tests — five bank layouts, rollover, date inheritance, sign healing,
  both Pass-5 recoveries, a whole demo year, ledger (dedup, transfers, card payments, business
  expenses, backups/restore/erase, rescan keeping edits), rules and truncation matching, amount and reference rules, splits,
  attachments, coverage, directory and keywords, insights, subscriptions, exports. Plus `-- --ignored real` re-scanning the owner's
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
| Statement coverage & continuity | Coverage grid + balance chaining; prior-period comparison only with coverage. Found the period parser reading the pay-by date (fixed) |
| Adriana CI$ 125 only | Amount-limited rules; reference-number rules for standing orders |
| Split charges, director's loan | 1-to-N splits; "owes you" cards + Record Repayment |
| Receipts | Local attachments by drop or button; P&L with receipts export |
| Landed cost | Tags as projects; freight & duty suggestions |
| Couldn't change charges in "owes you" | ✎ per charge opens the transaction editor; the dialog reopens with fresh totals |
| Where's the business database? | Business Directory page: search, seen-in-statements count, keywords, Override… → rule |
| Budget: crash on Fill, over count, 85% alert, pay-based wizard with projections | RefCell borrow held across redraw (fixed); badge + Overview card; 4-step wizard, plan, trend projection |
| Budgeting from past spending | Budget page: 6-month median suggestions, per-view budgets, pace projection |
| Consolidated statement | Combined Statement page: chosen accounts, months, combined balance, click to edit |
| CUC summer bills, annual renewals | Year-over-year spike check; Annual renewals card |

---

## 13. Outstanding / known limits

- **Four of five banks unverified on real PDFs** (Butterfield, CNB, RBC, CICSA): first real
  statement may need profile tuning (Bank Profiles → Test a PDF).
- **Directory accuracy**: most entries came from research agents with little web access; generic
  keys were removed, but individual entries may be wrong or closed. Harmless if absent from your
  statements; fix by editing `data/merchants.tsv` (rebuild needed) or with a rule.
- Year-over-year needs 12 months of history; annual renewals need the previous year's charge.
- Receipts are files, not OCR'd; the P&L-with-receipts export is a folder, not one merged PDF.
- A recorded repayment can't be undone from the UI yet (core has `undo_reimbursement`).
- No OCR for scanned PDFs; no font metadata from the PDF.
- The directory is read-only in the app (Business Directory page); additions are rules or edits to
  `merchants.tsv` plus a rebuild.
- The Python prototype's ledger was never migrated (statements re-imported instead).
- Not committed to a remote; no CI.
