# FinGraph — PRD / System Reference

**Repo:** `~/Projects/fingraph-rs` (Rust 2024, GTK 4 + libadwaita) · **Package:** `fingraph` 0.2.7 `.deb`
**Owner:** alex · **Doc written:** 2026-10-27 (as built; 0.2.0 budgeting release the same day) · **Updated:** 2026-10-28 for 0.2.7 · **Status:** in daily personal use; not published

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
- Image-only (scanned or flattened-by-redaction) PDFs are read by local OCR (`tesseract`, a
  recommended package) and never fully trusted: confidence ≤ 90% and "(read by OCR)" on the name.
- Not multi-user, not double-entry accounting. Budgeting is plan-based (Monarch-style budgets with
  rollovers, YNAB-style money moves and non-monthly bills), not strict envelope assigning of cash:
  statements arrive a month late, so the cash figure to assign from is always stale.
- No manual entry of day-to-day spending: everything comes from statements. Estimates between
  statements (Outlook) say they're estimates.

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
| `fingraph-core/src/parser/` | 5-pass scanner: `multipass.rs` (passes 1–3, layout notes), `reconcile.rs` (4–5), `profiles.rs` (bank profiles, fingerprinting, declared layouts), `dates.rs`; `mod.rs` holds the OCR fallback and anonymised layout samples |
| `fingraph-core/src/db.rs` | SQLite ledger (WAL), migrations, import planning and review holds (§6.2), transfers, card payments, rules, backups |
| `fingraph-core/src/rules.rs` | Merchant stems, learned rules, truncation-tolerant matching, Smart Triage grouping |
| `fingraph-core/src/merchants.rs` + `data/merchants.tsv` | Merchant directory (582 entries) and keyword rules |
| `fingraph-core/src/insights.rs` | KPIs, chart series, subscriptions, "What changed?" cards, balances |
| `fingraph-core/src/budget.rs` | Budgets (with history), rollovers, moves, envelopes, non-monthly bills, watchlists, the pay plan, month closes |
| `fingraph-core/src/outlook.rs` | Age of Money, regular payments and the bills checklist, balance forecasts and their checks |
| `fingraph-core/src/wealth.rs` | Net worth (account balances + manual items), goals and progress, debt payoff, rate estimates |
| `fingraph-core/src/learn.rs` | Naive-Bayes category guesser trained on the ledger (To Review suggestions) |
| `fingraph-core/src/export/` | CSV, QIF, OFX 1.0.2 SGML, QBO, P&L CSV |
| `fingraph-core/src/prefs.rs` | Preferences (stored in the ledger, so they travel with backups) |
| `fingraph-core/src/demo.rs` | Simulated statements for all five banks (tests + Load Demo Data) |
| `fingraph/src/*.rs` | One file per page/dialog; `charts.rs` is the Cairo chart widget; `main.rs` the shell — sidebar order, `OFF_SIDEBAR` pages reached from the menu, actions, CSS, and the ⓘ helpers (`with_info`, `info_heading`, `info_caption`) |
| `fingraph/src/guide.rs` | Getting Started: the eight-step guided setup (first run and main menu) |

Text and positions come from poppler's `pdftotext -bbox` (no font metadata; geometry does the work).
`examples/scan.rs` prints a scanner report; `examples/ledger.rs` runs reset / rescan / reapply / cards
from a terminal.

---

## 5. Data model (schema v8)

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
top-level category, monthly amount + currency) · `budget_history` (scope, category, month it applies
from, amount; `budgets_in(month)` reads the latest at or before a month) · `budget_moves` (money moved
between two categories in one month) · `sinking_funds` (non-monthly bills: amount, every n months, next
due) · `month_closes` (scope, month, summary JSON) · `manual_items` + `manual_values` (assets and debts
valued by hand over time, optional rate) · `goals` + `goal_entries` · `settings` (incl. the preferences
JSON, and JSON settings for rollovers `budget_rollover:<scope>`, watchlists `watchlists:<scope>`,
rate overrides `apr:<account>`, payoff amount `payoff_monthly:<scope>`, `forecast_checks`).

- **FITID** = SHA-256 of account, date, amount, normalised description and same-day sequence:
  re-importing a statement or an overlapping one never duplicates.
- **Workspace amount**: a rate printed on the statement (`USD 145.00 @ 0.84`, `(59.14 KYD)`) beats
  the configured rate (switchable).
- **Splits**: the lines must add up to the bank's amount; the statement row keeps its FITID and running
  balance (reconciliation is unchanged), while category totals, P&L, projects and "owes you" read the
  lines. Transfers can't be split. Splits, reimbursements and attachments follow the FITID through rescans.
- **Attachments** are copied (named by content hash) into `~/.local/share/fingraph/attachments/`.
- Migrations: v1 base, v2 business-expense columns, v3 hidden accounts, v4 amount rules, splits,
  reimbursements, attachments; v5 budgets; v6 budget history (existing budgets copied in from
  2000-01-01), moves, non-monthly bills, month closes, manual items, goals. Missing categories are
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
are verified against simulated layouts only (see §13), except **Cayman National**: a real
(redacted, image-only) Feb 2026 savings statement reads 95 of 95 transactions and reconciles to
the cent via OCR; its layout is pinned by `cnb_real_layout_multiline_rows_and_page_split`.

Imported PDFs are kept (by SHA-256) in `~/.local/share/fingraph/statements/` so **Rescan** can
re-read everything; a rescan keeps hand edits (categories, payees, tags, business flags,
inspector-converted lines) matched by FITID, keeps each statement in its account, and never replaces
a statement with a read that found nothing.

### 6.1 Declared layouts (0.2.4)

Each bank profile writes its layout down instead of the scanner guessing it at every read
(`LayoutRules`, stored as JSON in `bank_profiles.layout`, editable in Bank Profiles → Declared layout):

| Rule | Scotiabank | Cayman National | Butterfield / RBC / CICSA |
|---|---|---|---|
| Row shape | amount on a transaction's first line, detail lines below | date on the first line, amount + balance on the last | not checked yet (either accepted) |
| Column headings | chequing `Transaction Date · Description · DEBIT · CREDIT · Balance`; cards `TRANSACTION DATE · POSTING DATE · REFERENCE NO. · TRANSACTION / DESCRIPTION · DEBIT/CREDIT AMOUNT` | `Date · Cheque # · Description · Amount · Balance` | guessed |
| Summary labels | Opening/Previous Balance, Closing/New Balance, Withdrawals / Purchases…, Deposits / Payments & Credits | Beginning Balance, Ending Balance (on an `as of` line), Cheques & Other Debit, Deposits & Other Cred | generic |
| Checked on real statements | Checking, CreditCard | Savings (one OCR'd statement) | none |

A read that departs from its bank's declaration gets **layout notes** in the Scan Pass Report (a dated
line with no amount under an amount-first bank; none of the bank's headings found; a balance found
only under a generic label). That matters because a statement can reconcile and still be wrong: with
the wrong row shape every amount still adds up but lands on the line above, so names and dates shift
(`a_layout_mismatch_names_the_rule`).

### 6.2 Import rules (0.2.4)

1. **Plan first** (`Db::plan_import`): which known account each section matches, anything overlapping.
2. **Accounts match on bank + last 4 digits + currency.** The detected account type only breaks ties
   (OCR read a CNB savings statement as Checking). No readable account number → no match; the person
   picks from that bank's accounts in that currency.
3. **Ask before anything new.** A new account ("Is this your account?"), an unreadable account number,
   or an account + period already imported (another copy: replace or skip) is asked in the import flow.
   The watched folder imports only what needs no answer; anything else waits, offered again until decided.
4. **Hold what isn't proven** (`documents.review_hold`, reasons in `review_note`): doesn't reconcile, no
   balances, read by OCR, the first statement of its account type from its bank, or off its declared
   layout. Held statements and their transactions count nowhere (every query filters them; `documents()`
   leaves them out, `documents_all()` keeps them for the Statements page and coverage) until **Approve**.
   Approving a first-of-its-kind statement adds that type to the bank's verified list and releases the
   others held only for that; fixing a mismatch in the inspector releases it by itself.
5. **Fingerprints without descriptions:** FITID = account + date + amount + which of that day's
   same-amount rows it is. Existing ledgers are re-keyed once (`fitid_v2`), repayments and receipts carried
   across. A statement with neither transactions nor balances is refused.

### 6.3 Bringing Butterfield, RBC and the Credit Union up to accuracy

Nothing about these three is known from a real statement yet, so accuracy comes from real samples,
in four gated steps per account type:

1. **Collect** one statement per account type, downloaded as a PDF from online banking (not printed and
   scanned, not redacted — redaction flattens pages to pictures; the layout sample below removes the
   private parts instead). Two consecutive months of one account, a statement of 2+ pages and one with a
   foreign-currency purchase where possible.
   - Butterfield: chequing, savings, Visa. RBC: chequing, savings, card. CICSA: the combined statement
     (shares, loans, chequing in one PDF).
2. **Test without importing:** Bank Profiles → Test PDF. Gate: recognised as the bank, reconciled to the
   cent, no layout notes.
3. **Save Layout Sample…** from that result: every word keeps its position; dates, amounts, headings,
   summary labels and the bank's own identifiers stay; names, merchants, account and reference numbers
   become `x`/`0` (checked: all 35 real Scotiabank files and the OCR'd CNB statement read identically from
   their samples, with no merchant or person names left). With the sample the profile is set — date order,
   amount style, row shape, headings, summary labels — and the sample goes in
   `crates/fingraph-core/tests/fixtures/private/` (git-ignored), where `private_layout_samples` must keep
   passing.
4. **Confirm in the app:** the first import of each account type is held (new layout); compare its dates,
   amounts and a few descriptions with the PDF and Approve. That type is then verified, and later
   statements import by themselves while they reconcile and stay on layout. A bank that changes its
   layout later gets layout notes, so the change is held rather than imported.

What to look for, from what's known so far:
- **Butterfield** — online banking shows separate *Value Date* and *Transaction Date* columns, Credit
  and Debit columns, and text-month dates (`14 Feb 2023`): which date is the ledger date, and whether the
  PDF matches the screen. Visa: FX lines, `CR` on credits.
- **RBC** — expected to print the date only on a day's first row and the balance only on its last, so
  fewer balance checks per statement; a "balance forward" opening row; cards on a different layout.
- **CICSA** — several accounts in one PDF: each section needs its own opening and closing; loans are
  liabilities (a payment lowers the balance); member number vs account numbers decides account matching.

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
4. **Merchant directory** (`data/merchants.tsv`, 582 entries: Cayman supermarkets, liquor, pharmacies,
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

**Learned guesses** (`learn.rs`, Copilot-style): once ≥ 30 transactions have a category, a naive-Bayes
model (merchant words, amount band in powers of two, weekday, account, direction) suggests up to two
categories (≥ 15% likely) per To Review card. They lead the chips, highlighted, with a tooltip; nothing
is filed automatically. Held-out check on the demo ledger: 77 of 79 in the top two.

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
  (donut → merchant drill-down), cumulative daily spend vs the usual band, "Where Most of Your Money Goes" (top-merchant Pareto),
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
  A known service (Netflix, Spotify, Disney+, YouTube, Audible, Prime, Adobe, Microsoft, Apple,
  Google, Claude…; `merchants::manage_url`) gets a button opening its manage/cancel page.
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
  - **Fixed vs Variable**: each category is fixed or variable (a pill on its row toggles it; saved
    as the `budget_fixed` setting, defaulting to Bank Fees, Education, Government, Housing,
    Insurance, Property Expense, Subscriptions & Digital, Utilities). A bar splits take-home into
    fixed / variable / loans / saved (or fixed vs variable without a plan), and a card each lists the
    categories (budget, else the usual cost) with this month's spending and share of take-home. The
    wizard's trim applies to variable categories.
  - **Setup wizard** (Set Up Budget… / Edit Plan…), 4 steps: pay (cycle weekly / fortnightly /
    twice a month / monthly, next payday, gross, deductions as amount or % of gross; prefilled from a
    detected salary — same payer ≥ 3× in 120 days at a steady rhythm and amount, payroll wording
    preferred), other income and loan payments (median of 6 months of Loan Payments), category
    budgets (fixed bills vs flexible; a Monthly | Fortnightly switch shows them per fortnight —
    × 12 ÷ 26 — and saves monthly, exactly, when unchanged), savings goal (amount or % of take-home). Shows take-home −
    spending − loans = savings and, when short, solves take-home − loans − fixed − k·flexible = goal
    for k and offers to trim flexible categories (to the nearest 5). Plan saved per view as JSON
    (`budget_plan:<scope>` setting).
  - **Your Plan** card + **12-month savings projection**: on budget (constant monthly savings) vs on
    trend (each category's 6-month least-squares line carried forward, floored at 0) vs goal; next
    three paydays (monthly pay keeps its day, clamped to month end). Rows note a trend of ≥ 5%
    (min CI$ 5) a month.
  - **Budget history**: a budget applies from the month it was set; a category's first budget also
    covers earlier months (so last month can be closed against it). Past months show the budget
    they had.
  - **Rollovers** (Monarch): per category from a month on (⋯ menu); each month's leftover — or
    overspend — carries into the next (`envelopes`, walking back at most 24 months). Row subtitle
    shows "rolled over" / "overspent last month".
  - **Move money** (YNAB rule 3): ⋯ → Move Money In / Out / Cover Overspending…, one month only, from
    the category with the most left by default; an overspent category is offered exactly its
    overspend. Undo on the same menu. Alerts, bars and Left use budget + rollover + moves.
  - **Statements arrive late**: "how far through the month" for alerts and Heading For is measured
    to the newest statement's end date, not today; a month with no statement yet says so.
  - **Non-monthly bills** (YNAB true expenses / Monarch non-monthly): amount, every 3/4/6/12/24 months,
    next due; a share a month, "set aside by now" = one share per month of the cycle so far (the
    month after the last due date is the first). Suggested from the statements: bills at a steady
    91/365-day rhythm, and yearly/quarterly subscriptions ("Set Aside For It"). The monthly total
    comes off Your Plan's savings and shows as its own part of the Fixed vs Variable bar.
    Since 0.2.7 the setup wizard asks for them (step 3, **Big Yearly Bills**, see the decisions log),
    and Overview shows **Big Bills Coming Up** and **Big Bills Over the Next 12 Months**
    (`budget::fill_big_bills`).
  - **Watchlists** (Simplifi): a monthly limit on any wording (matched in description, memo and
    payee), across categories; over / near limit joins the budget alerts and badge; list button
    shows the matching transactions.
  - **Where the Money Went** (Monarch Sankey): income sources (top 5) → one bar → categories
    (top 9) + Saved (or From savings on the left); labels thin out when bands are small.
- **Monthly Close** (Budget → Monthly Close…, dialog): pick a month (✓ = closed; default the newest
  month that's over and not closed). Steps: Statements (each account's coverage for the month) →
  Sort What's New (uncategorised count, Start Smart Triage) → Budget (overspent first, Cover…) →
  Bills (the checklist below) → Goals (progress; record contributions to unlinked goals) → Close the
  Month (money in, spending, saved and rate, Age of Money, net worth, over-budget count, bills
  missed; earlier closes for comparison; Close / Close Again / Reopen). Closing also remembers the
  balance forecast for the end of next month.
- **Outlook** page (between statements):
  - **Balance forecast** per bank account (not cards/loans): from the last verified balance, regular
    payments in and out (incl. transfers and card payments) on their rhythm, plus everything else at
    the last 90 days' average per day; 45 days ahead; chart (from at most 30 days back), per-account
    "≈ today", low point, next payment, and — once a statement covers a remembered forecast date —
    "estimated X, the statement says Y (off by Z)". Coming Up: the next 30 days of regular payments.
  - **Regular payments** (`regulars`): same account + merchant + direction at a 7/14/30/91/365-day
    rhythm, flagged recurring/subscription, or ≥ 3 charges within 25% of the typical amount (median
    of the last three).
  - **Bills checklist** (`bills_for`, also in Monthly Close): Paid (dates), **Missed** (the account's
    statement covers the due date and no charge), **Not yet seen** (statement not in), New. A bill
    due in the last 4 days and charged just after the month counts as paid; three skipped cycles =
    stopped.
  - **Safe to Spend** (PocketGuard, adapted): variable categories' money left this month (budget +
    rollover + moves − spent) ÷ days from the newest statement to month end, with a reminder that
    spending since the statement isn't in yet.
  - **Lowest Balance Ahead**: an account heading below zero, else the account most bills go through.
- **Age of Money** (YNAB rule 4): income into bank accounts (not transfers) stacked oldest first,
  each statement's opening balance counting from its first day; every purchase (cards included, at
  purchase) spends the oldest money; age = average of the last 10 purchases; month-end series.
  Overview tile, Outlook tile + chart with a 30-day line, Monthly Close summary.
- **Net Worth** page (Monarch): ribbon (net, assets, debts, change over 12 months), chart from the
  first month with data (net, assets, debts), accounts with their last verified balance, **Other
  Assets & Debts** (added by hand, valued over time — Update Value…, optional rate).
  - **Goals**: save up or pay down; amount, monthly, optional date; follow an account (save: its
    balance; pay down: owed at the start − owed now) or record contributions (Monthly Close).
    Status: Done; with a date — At risk if the planned amount finishes after it, Ahead if it needs
    ≤ 80% of the plan, else On track; without — pace since creation (≥ 110% Ahead, ≥ 90% On track).
  - **Paying Off Debt** (PocketGuard): cards/loans owing money + hand-added debts; rate estimated
    from the last three months of interest charges against what's owed now (typed rate wins,
    `apr:<id>`); minimum 3% of the balance, at least CI$ 25; avalanche vs snowball side by side
    (debt-free month, interest, payoff order) for the monthly amount (default the plan's loan
    payments, else the minimums).
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

**Sidebar** (`AdwNavigationSplitView`; order since 0.2.5 at the owner's request: Overview, Net Worth,
Budget, Outlook, Transactions, Combined Statement, Statements, What Changed, Subscriptions — Ctrl+1–9
in that order): each page has a white symbolic icon on a coloured rounded
tile (Setup pages slate); icons are all from Adwaita so none fall back to full-colour images; workspace switcher (one workspace or Consolidated across a
chosen set) at the top; badges on What Changed *(count)*, Subscriptions *(monthly cost)*,
Statements *(⚠ when one doesn't balance)* and Budget; **Accounts** — each with its last verified balance (owed in red), opening its overview;
listed last. Since 0.2.5 the **main menu** holds Import Statements, Reports & Export and the setup pages — Rules & Categories, Business
Directory, Bank Profiles, Preferences (Ctrl+,) — and **To Review** is reached from a banner
across the top of Overview ("N merchants to review" · Review), shown only while something waits.
The header shows only the controls a page uses (date range, KYD | USD). Reopens on the last page.

**ⓘ on every card** (0.2.5; `with_info`/`info_heading`/`info_caption` in `main.rs`): the KPI tiles
and charts on Overview, and the cards and sections of Budget, Outlook and Net Worth, carry an ⓘ
after their title — hover shows how to read it, a click keeps it open in a popover.

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

**Getting Started** (`guide.rs`; opens on first run, and from the main menu → Getting Started; also
after Erase All). A dialog with the eight steps listed on the left — ticked when their work is done
(from the ledger, not from clicks), any of them reachable at any time — and Back / Next below. It
remembers the step you were on (`guide_step`); closing it early is fine.
1. **Welcome** — what FinGraph does, the five-step routine with honest timings, privacy, "a month at
   a time" (no bank feeds in Cayman), and Load Demo Data with the warning that it mixes with real
   statements (clear it with Erase All, which now clears budgets, goals and the other v4–v6 tables too).
2. **Who It's For** — the household workspace, an optional business/rental one, and the display
   currency; renames the existing workspaces rather than adding duplicates.
3. **Download Statements** — which banks (five known + "another bank", remembered as
   `guide_banks`); for each, where to look (generic: Statements / eStatements, PDF — no invented
   menu paths); what to get: every account incl. each card, the PDF statement not a CSV or history
   export (it has the balances FinGraph proves against), 12 months (≥ 6), the files as the bank
   gives them (no print-to-PDF or scans). An unknown bank is pointed at Bank Profiles → New from Sample.
4. **One Folder** — use `~/Documents/Bank Statements` (created) or choose any folder; shows how
   many PDFs it holds and how many are new; Open in Files and Look Again; how to drag files over
   from Downloads; **Check this folder for new statements** (the watched folder; subfolders are
   read too, now that `pdfs_in` is recursive). Continue needs at least one PDF (or statements already).
5. **Read Them** — reads every file not read before (by path, size and time) with a progress bar
   and one row per file (bank, accounts, period, transactions, Adds up / Off by / Couldn't read),
   then: statements · accounts · transactions and the date span; money in and out with a monthly
   average (transfers between your own accounts left out); statements that didn't add up and how to
   fix them (Scan Pass Report → Convert); months covered per account (● ◐ ○) and what to do about
   gaps; Read Again. Files read here are remembered so the watcher doesn't scan them again, and the
   usual post-import triage jump is skipped (`import_paths_with(.., on_done)`).
6. **Sort Transactions** — how many were sorted automatically, how many are moves between your own
   accounts (never income or spending), how many wait; then the To Review cards inline, one at a
   time, the biggest money first, with Skip This One and a running count; stopping is fine (the
   rest wait in To Review).
7. **Your Budget** — what you usually spend per category (Uncategorized shown apart, as "isn't
   sorted yet"), the pay found in the statements, a warning under three months of statements, the
   four setup steps, and Set Up My Budget… (the budget wizard opens over the guide and moves it on
   when saved).
8. **Every Month** — the routine: download last month's statements into the folder (read on the
   next start when watched), Monthly Close, Outlook in between; Net Worth, Subscriptions, a calendar
   reminder for the 5th. Finish starts watching the folder and shows the Overview.

Fixes found while testing the guide on the owner's real statements:
- **Budget suggestions** use only months the statements cover completely (months before the first
  statement or after the last are unknown, not zero); with less than one whole month, the partial ones.
- **Pay detection** looks back from the newest statement (not today), groups deposits ignoring
  reference numbers (`rules::payer_key`: "100000727, ISLAND AIR…" and "100000928, ISLAND AIR…" are
  one payer), and needs only half the deposits near the usual amount (a bonus, refunds or a penny
  test under the same wording no longer hide the salary). Regular payments (Outlook, bills
  checklist) and non-monthly bill ideas group the same way.

---

## 10. Packaging & operations

- `cargo deb -p fingraph` → `target/debian/fingraph_0.2.7-1_amd64.deb` (binary, desktop entry with
  PDF "Open With", icon, README; depends on poppler-utils, GTK 4, libadwaita; recommends
  tesseract-ocr for picture-only statements).
- Install: `sudo apt install ~/Projects/fingraph-rs/target/debian/fingraph_0.2.7-1_amd64.deb`. The
  version is bumped for every change to the app: apt skips a same-version `.deb` unless given
  `--reinstall`, and once that made a fix look missing. A docs-only rebuild keeps the version and
  is installed with `--reinstall`.
- Releases are tagged on master (annotated `v0.1.0` … `v0.2.6`). `v0.1.0` marks the last commit
  before 0.2.0, and `v0.2.2` shares `0d049d5` with `v0.2.3` because the two were committed together.
  The repo has no remote, so tags are local.
- Data: `~/.local/share/fingraph/ledger.db` (+ `statements/`, `backups/`). Hand backups made during
  development sit beside the ledger as `ledger-backup-2026-10-27-*.db`.
- Terminal: `cargo run -p fingraph-core --example ledger -- reset|rescan|reapply|cards`.
- Debug builds only: `FINGRAPH_SHOT=out.png` (with `FINGRAPH_DB`, `FINGRAPH_DEMO`, `FINGRAPH_VIEW`,
  `FINGRAPH_DIALOG` (`triage|onboarding|report|preferences|budget|close|split:<id>|owed:<id>|tx:<id>`), `FINGRAPH_PAGE`, `FINGRAPH_SHOT_STEPS`, `FINGRAPH_SCROLL`, `FINGRAPH_CLICK=label,label…` clicks buttons in turn) renders the window to
  a PNG for visual checks without a person at the screen. The hooks can't hover or open menus and
  popovers.
- **README screenshots** (`docs/screenshots/`: overview, guide, statements, review, budget, outlook,
  networth, transactions, statement) come only from a fresh scratch ledger
  (`FINGRAPH_DB=<scratch> FINGRAPH_DEMO=1`), never the owner's: watching is off in a new ledger, so
  nothing from the watched folder can leak in. Budget is shot after `FINGRAPH_CLICK="Fill From My
  Spending"`. Check the scratch ledger holds only the demo workspaces and accounts, look at every
  image, then delete the ledger. The images stay out of the `.deb`.

---

## 11. Quality

- `cargo test --workspace`: 70 tests (0.2.7: a bill picked by its due month lands on the next one; 0.2.3–0.2.4 added the real CNB layout and OCR, multi-line
  rows, a layout mismatch that still reconciles, anonymised layout samples, import plans and holds,
  FITID re-keying and the one-time profile refresh; the guide release added whole-month suggestions and salary-with-a-bonus; 0.2.0 added rollovers/moves/history, non-monthly bills,
  watchlists and closes, Age of Money incl. a hand-checked FIFO case, the bills checklist incl.
  missed vs not-yet-seen, forecasts and their checks, net worth, goal status, avalanche vs snowball,
  rate estimates, the guesser's held-out accuracy, manage links) — earlier: — five bank layouts, rollover, date inheritance, sign healing,
  both Pass-5 recoveries, a whole demo year, ledger (dedup, transfers, card payments, business
  expenses, backups/restore/erase, rescan keeping edits), rules and truncation matching, amount and reference rules, splits,
  attachments, coverage, directory and keywords, insights, subscriptions, exports. Plus `-- --ignored real` re-scanning the owner's
  statements, and `-- --ignored private` for layout samples in `tests/fixtures/private/`.
- `cargo clippy --workspace --all-targets` clean; `cargo build --release` clean.
- UI checked page by page with the screenshot hooks against the demo and a copy of the real ledger.
  The hooks can't hover or open popovers, so the ⓘ text and menu items are checked by hand.

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
| Budget icon red, others grey; fixed vs variable breakdown | `wallet-symbolic` didn't exist (fell back to a colour icon): every page now has a coloured tile with an Adwaita icon. Fixed vs Variable section + per-category switch |
| Monthly / fortnightly in the wizard's spending step | Toggle; amounts convert at 26 fortnights ÷ 12 months, saved monthly |
| "Subscriptions CI$ 860 a month" in the budget vs CI$ 161 on Subscriptions | The category also holds one-off digital purchases; its suggestion is now active subscriptions + the usual one-offs, labelled as such. Gross shown per payday and per month |
| Budget: crash on Fill, over count, 85% alert, pay-based wizard with projections | RefCell borrow held across redraw (fixed); badge + Overview card; 4-step wizard, plan, trend projection |
| Budgeting from past spending | Budget page: 6-month median suggestions, per-view budgets, pace projection |
| Consolidated statement | Combined Statement page: chosen accounts, months, combined balance, click to edit |
| CUC summer bills, annual renewals | Year-over-year spike check; Annual renewals card |
| Best of YNAB, Monarch, Copilot, Simplifi, PocketGuard, Rocket Money — statements come monthly (0.2.0) | Monarch-style plan + rollovers, YNAB moves, non-monthly bills and Age of Money; Monthly Close; Outlook (forecast, safe to spend, bills checklist); Net Worth, goals, avalanche/snowball; learned guesses; watchlists; Sankey; manage links. No manual spending entry (statement-driven by choice); bill negotiation, credit scores, live sync, sharing, AI chat left out |
| Monthly Close for last month showed "No budget yet" | A category's first budget covers earlier months; later changes apply from their month |
| Dated goal "At risk" while finishing before its date | A dated goal is judged by finishing in time (and Ahead when it needs ≤ 80% of the plan); pace only without a date |
| Kept freezing ("Not Responding") on every launch | Debt payoff compounded a 266% estimated rate for 50 years until Decimal overflowed; it now stops once a month leaves as much owed as the last ("interest grows faster than this pays"). Any panic now goes to `crash.log` in the data folder, and a page that fails shows a toast instead of closing the app |
| Import rules (0.2.4) | Accounts match on number not detected type; new accounts, unreadable numbers and re-imported periods are asked; statements that don't reconcile, were OCR'd, are first of their kind or off-layout are held until approved; FITID no longer includes the description (re-keyed once). Found on the owner's ledger: one Amex USD statement imported from two files counted 7 rows twice (US$919.61) because the copies wrapped descriptions differently; a rescan under 0.2.4 drops them. Scotiabank's headings in the profile were guesses (`Withdrawals (-)`, `Total Minus`) — replaced with the real ones; declared layouts per bank (§6.1); anonymised layout samples (§6.3) |
| CNB from a real statement (0.2.3) | The guessed CNB profile was wrong on every point. Real layout: `DD/MM` dates, one Amount column with a trailing minus, the date on a description's first line and the amount + balance on its last (Pass 3 now holds a dated amount-less line as a "head" that later lines join, across a page break too), summary amounts on an `as of` line under spaced-out labels, `ACCOUNT #: SV` = savings. Stored built-in CNB profiles are replaced once (`profile_rev:cnb_ky`). OCR fallback: 300 dpi grey, pale ink blanked at 180 (the CUSTOMER COPY watermark garbled digits), `--psm 6`, `49,48`→`49.48`; one printed balance that disagrees with both neighbours is ignored and noted ("Balance ignored") instead of failing the stretch |
| Snappier (0.2.2) | Transactions 1.9 s → 0.1 s per refresh (cell buttons instead of a DropDown/EditableLabel per row; only changed rows are replaced); refreshes coalesce to one per frame (start-up did 5); rows read once per refresh; sidebar 75 → 30 ms; Subscriptions 109 → 7 ms; Combined Statement skips rebuilding when nothing changed; index on `transactions(document_id)` (schema v7); `synchronous=NORMAL` under WAL |
| Sidebar and ⓘ (0.2.5) | Owner asked for To Review to disappear when empty — it became a banner on Overview instead; Setup pages moved to the main menu; an ⓘ explaining how to read each card on Overview, Budget, Outlook and Net Worth. "Top Merchants · Pareto" renamed "Where Most of Your Money Goes". Long card titles now wrap: the ⓘ had pushed Overview wider than the window |
| Sidebar order (0.2.6) | Owner's order: Overview, Net Worth, Budget, Outlook, Transactions, Combined Statement, Statements, What Changed, then Subscriptions (not named, put last); Ctrl+1–9 follow it. Also in this round: Reports & Export moved from the sidebar to the main menu, and Load Demo Data left the menu (it stays on the empty Overview and in the guide). Versions now bump on every `.deb`: a same-version rebuild was once installed over without `--reinstall` and the change looked missing |
| README screenshots (0.2.6, docs only) | Owner asked for pictures of the app in the README with none of their data: nine pages shot from the demo in a scratch ledger (see §10); past releases tagged at the owner's request |
| Big yearly bills (0.2.7) | Owner: "does the software ask what are your once a year costs?" It didn't. Non-monthly bills existed only on the Budget page, and suggestions need two charges a year apart from the same payee. The budget setup gains step 3, **Big Yearly Bills**: 9 common ones (car / home insurance, HOA or strata, vehicle licence, health insurance if self-paid, school fees per term, memberships, travel, Christmas & birthdays) plus bills already planned and ones found in the statements, each with amount, how often (yearly, 6, 4 termly, 3 months, 2 years) and the month it's due. The set-aside comes out of "left to save", and is treated as fixed when trimming variable spending to reach the goal. An emptied row deletes that bill. Overview gains **Big Bills Coming Up** (next five, set aside so far) and **Big Bills Over the Next 12 Months** (what lands each month), or an Add Big Bills… card when none are planned. Getting Started's budget step lists the five steps and what's planned or found |

---

## 13. Outstanding / known limits

- **Three of five banks unverified on real PDFs** (Butterfield, RBC, CICSA): follow §6.3. Until then
  every statement from them is held for review on import.
- Ledgers from before 0.2.4 may hold rows counted twice where one statement was imported from two
  files (the owner's: 7 Amex USD rows, US$919.61); Rescan All removes them. Nothing is deleted on upgrade.
- The import questions (new account, which account, same period), the ⓘ popovers and the main menu
  are tested in the core or by hand, not by screenshot. CNB is checked only against an
  OCR'd savings statement: a CNB text PDF, a chequing account or a CNB card may still differ.
- OCR reads the first 20 pages only, misses the bold `ACCOUNT #: SV` line next to a redaction box
  (the account shows as Checking), and can misspell names.
- **Directory accuracy**: most entries came from research agents with little web access; generic
  keys were removed, but individual entries may be wrong or closed. Harmless if absent from your
  statements; fix by editing `data/merchants.tsv` (rebuild needed) or with a rule.
- Year-over-year needs 12 months of history; annual renewals need the previous year's charge.
- Receipts are files, not OCR'd; the P&L-with-receipts export is a folder, not one merged PDF.
- A recorded repayment can't be undone from the UI yet (core has `undo_reimbursement`).
- No font metadata from the PDF.
- The directory is read-only in the app (Business Directory page); additions are rules or edits to
  `merchants.tsv` plus a rebuild.
- The Python prototype's ledger was never migrated (statements re-imported instead).
- Not pushed to a remote (tags are local too); no CI.
- 0.2.0 limits: rollovers walk back at most 24 months; the debt minimum is one rule (3%, ≥ CI$ 25)
  for every card and loan, so a loan's payoff is the slowest case; the forecast spreads everything
  irregular (including card payments that vary) evenly per day; Age of Money grows without bound
  when large balances sit unspent (as YNAB's does); weekly/fortnightly bills show as one checklist
  line per month; manual items belong to the first workspace of the view they were added in.
