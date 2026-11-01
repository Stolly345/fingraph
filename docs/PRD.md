# FinGraph — PRD / System Reference

**Repo:** `~/Projects/fingraph-rs` (Rust 2024, GTK 4 + libadwaita) · **Package:** `fingraph` 0.2.14 `.deb`
**Owner:** alex · **Doc written:** 2026-10-27 (as built; 0.2.0 budgeting release the same day) · **Updated:** 2026-10-31 for 0.2.15 · **Status:** in daily personal use; not published

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

## 5. Data model (schema v9)

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

### 6.4 When a bank changes its layout (learning)

Nothing about a layout has to be coded for a statement to be read: with no profile at all, the scanner
reads and reconciles all of the owner's statements. What a profile adds is knowing when a read is *right*,
and a bank that changes its statements breaks that. Four parts keep it working without code changes:

1. **The bank is named on page 1** (`profiles::fingerprint` needs a page-1 hit): a letterhead that names no
   known bank is an unknown bank, not whichever bank a transfer line mentions (a rebranded Scotiabank
   statement had been taken for CNB).
2. **Try, then prove** (`MultiPassScanner::scan_tokens`): a read that doesn't reconcile or is off its declared
   layout is read again, ignoring the profile's headings, labels and row shape; with day and month swapped;
   and **from the amounts alone** (`data_layout`: rows that open with a date, money columns where the
   amounts' right edges line up, each debit/credit/balance assignment tried). The read that reconciles
   with the most rows wins; one that doesn't reconcile never replaces the saved layout's read. The winner is
   still held: its Scan Pass Report says what the saved layout got wrong and how it was read instead.
   Guards: dates count only where most dates start (memo lines that open with a date don't move the
   column); a summary box above the table is never part of it; an undated total line (`Deposits & Other
   Credits`) is never recovered as a missing transaction, even when its amount fits (found on the demo).
3. **Learn on Approve** (`Db::approve_document` → `BankProfile::learn`): approving a held statement that
   reconciles teaches its bank profile the headings over each column, the opening/closing balance words,
   the row shape and the date order it showed. Only adds headings and labels, so older statements keep
   reading. The previous profile is kept (`layout_history:<id>` setting, last ten): **Bank Profiles → Undo
   Learned Layout**. The bank's other statements held for their layout are re-read with what was learned,
   and an anonymised layout sample is kept in `<data dir>/layout_samples/` (checked by
   `private_layout_samples` with the ledger's profiles).
4. **Changed since last month** (`ObservedLayout` in each statement's diagnostics): each import is
   compared with the account's last approved statement before it. Different headings, money columns, row
   shape or date order hold it as *the bank changed how its statements look*, with the changes listed.

Checked by `tests/drift_probe.rs` (ignored; your statements): renamed headings and labels, another date
format, no currency symbols, `DR` for minus, a moved page, a rebrand and an unknown bank all read the same
dates, amounts and descriptions as the unchanged statement; after one approval the changed statement
reads with no note and the unchanged one still does; a fresh import of every statement holds none; Rescan
All on a copy of the owner's ledger (34 files) holds nothing new. `a_changed_layout_is_held_once_then_learned`
covers the ledger side on demo data.

**Third-party samples** (`tests/online_samples.rs`, ignored, `FINGRAPH_SAMPLES=<dir>`; synthetic PDFs from public
test sets, read with no profile and imported only into an in-memory ledger): 6 of 8 read in full and reconcile —
three Indian accounts (AyonPal/bank-statement-pdf-parser, every date and amount matches its published answer),
and from the Bankstatemently benchmark a Singapore savings account, a US card (MM/DD dates, `0.00` detail
lines, a diagonal watermark) and a bilingual Hong Kong statement with two accounts in one PDF. With their
headings renamed, five read the same rows; the card (no balances, and the rename removed its summary
labels) reads the same rows unverified, one row a day off (which date column posts can't be told without
headings). Fixes this found: sign-heals ranked (swapped debit/credit columns add up one flipped sign at a
time when every row has a balance), summary labels above their amount in a side box, explicit closing labels
before "carried forward", day/month order from the dates for unknown banks, undated `0.00` lines are detail,
`HKD Current Account — …` starts a section, in-table `Total Deposit Amount` lines are that section's totals,
watermark letters dropped, a lone-amount-column assignment, and a read with no rows is replaced by the
re-read with the most.

**Owner-supplied samples (2026-10-30):** TemplateLab forms 1/2/4 (placeholder `mm/dd/yyyy` dates), SampleTemplates'
Dummy Bank Statement, an LP1 training PDF (statement + a farm ledger + a balance sheet) and a scanned sample
(transactions listed again by type on page 2). All three with real data read exactly (12/12, 8/8, 21/21) and
reconcile; the templates are refused ("amounts but no dates … a blank template?"). Found and fixed:
- **False "Reconciled"**: without a closing total, all balance checks passing was enough even when rows came
  after the last printed balance (29 ledger rows passed). Now the checks must reach the last row (a first row
  printing its own balance may go unchecked when no opening is found); a read of no rows with amounts left
  over is Unverified.
- **A lost first row**: a balance printed before the first transaction replaced the printed opening, so a first
  row whose amount was unreadable vanished and the rest still added up. The printed opening now comes first and
  the balance line is checked against it.
- Rows after the closing balance with no balances of their own (a re-listing by type, another document's table)
  are set aside when the statement then adds up to that closing balance; an in-table `Ending Balance` line is
  the closing balance.
- A dated line whose amount is unreadable but whose balance printed (OCR `-26` for `.26`) is worked out from the
  balances and held (**Review::Derived**). The demo's garbled row prints no balance, so it still shows the inspector.
- Dashes standing for empty amounts aren't description; words spilling into the date column go back to the
  description; `Detail` is a description heading.

**23 Word templates (owner's `~/Desktop/fakes`, converted to PDF with LibreOffice; answer keys written by hand
from each PDF):** read exactly and reconciled: 07 (30 rows, Naira, two date columns), 22 (Westpac 1997, 20), 25
(item numbers glued to amounts, 7), 27 (94 pages, table split between p1 and p93, 23). Every row right but held
because the template's own arithmetic is wrong: 09 (11/11), 26 (10/11, a page of activity missing). Not read
(held or refused, nothing imported): 08, 12, 17, 18 (decimals printed as spaces), 21 (year `20XX`), 30, and
partly 04, 19, 23 (newest first), 13 (a textbook chapter), 06 (a fee invoice). Found and fixed: a line printing
both a debit and a credit was netted into one transaction and still "reconciled" against the totals — it's two
transactions now, with a hold; dates without a year and no readable period took the current year silently —
noted and held; `Jan.1, 2013` reads as a date.

**Statement engine without a model (2026-10-30).** The scanner reads any layout by trying the readings a table
allows and keeping the one its own balances prove, measured by a generator (`tests/fuzz_layouts.rs`): statements
with random column orders and roles, English/unusual/French/Spanish/no headings, eight date formats, point/comma/
space-grouped amounts, minus/trailing-minus/parentheses/CR-DR signs, newest-first lists, wrapped descriptions,
summary boxes or balance lines, watermarks, margin lettering, page breaks and a re-listing after the statement,
each with its answer. Baseline before this work: 31–40% read exactly and proven; now 95–97% (4,000 statements,
two seeds), and none wrong yet marked Reconciled — the test fails on one, and on the default set falling under
94%. What changed:
- Amounts: each document's decimal style is found from its unambiguous amounts (`money::infer_decimals`) and the
  whole read uses it (`with_decimals`); thousands written with spaces are joined (`join_thousands`); `€ £`.
- Dates: French, Spanish, Dutch, German month names (not Portuguese/Italian `out`/`set`, which would read
  "Money Out 12.50" as a date); `02 janv. 25`. French/Spanish headings and balance labels.
- Newest-first sections are found from their dates and checked bottom-up (`mirror`); positions are put back.
- Anything after the printed closing balance — a re-listing, another document's table — is set aside where the
  running balance prints the closing, or with no running balance at a break in the page where opening + the rows
  above = closing; kept only if the statement then adds up.
- Safety found by the generator: the printed opening and closing come before balance lines (a first row whose
  amount strayed had become the opening, its deposit lost); an amount is worked out from the balances only when
  an unreadable amount is visibly on the line (a re-listing's figures had been turned into four invented rows);
  without an opening balance the first row is unproven unless its column gives its direction and the balances
  confirmed that reading everywhere else (3+ checks, no sign corrected) — a lone unsigned column had let a first
  withdrawal "reconcile" as a deposit.
- A card read as a chequing account flips every sign and still adds up: the arithmetic can't tell, so the account
  type from the letterhead (credit limit, minimum payment…) is what decides. Real card statements always print it.

**0.2.10: four checks from an outside review** (owner pasted another AI's suggestions; agreed with these four,
declined its multi-sign "subset-sum" solver: using the balances to choose many signs spends the check that
would prove them, so "Reconciled" would stop meaning anything).
1. **Continuity** (`Db::previous_closing`, at import): each statement must open where the account's previous
   approved, reconciled statement (ending within 3 days before it starts) closed. A printed opening that doesn't
   is held (`Review::Continuity`: a statement or rows may be missing); a statement printing no opening is checked
   from the previous closing and kept that way only if every row then adds up. All of the owner's 41 statements
   chain exactly.
2. **Card or bank account** (`polarity_note`): fees raise a card's balance and lower a bank account's; a
   "payment received" does the opposite; salary raises a bank account's. Two or more lines that disagree with
   the type read hold the statement (the type isn't changed). And a statement that doesn't say what account it
   is (`type_unstated`: no card wording, no "savings account" and the like — "Savings Bank" in a name doesn't
   count) and prints no running balance is held: a card and a bank account read the same amounts in opposite
   directions and both add up. The generator now makes cards that don't say so; every one is held.
3. **Transaction vs posting date** (`transaction_and_posting_swapped`): a transaction never posts before it
   happened, so when most lines have the "posting" date first the columns were taken the wrong way round and are
   swapped (the ledger date stays the posting date). Recovered lines use the posting date too (they had used the
   other one). The generator prints two dates in either order, with and without headings.
4. **Scanned amounts read again** (`reread_cell`, scans only): an amount worked out from the balances is
   confirmed when its cell, rendered alone at 400 dpi and OCR'd as one line of digits, shows exactly its digits
   (`-26` confirms 0.26: OCR loses a faint point, it rarely invents digits); a gap is filled from a re-read cell
   only with a well-formed amount that the balances then accept. Derived amounts are tracked by position
   (`SectionReport::derived`); confirmed ones no longer hold the statement. The scanned sample's `.26` is
   confirmed this way.
Generator: 90–92% of all, 94–96% of provable statements read exactly (harder set: two-date layouts and
unmarked cards added), none wrong and passed through — "proven" now also needs no notes.

**0.2.11: adversarial tests** (owner pasted another AI's test ideas; built those that weren't already covered).
Generator options added: "balance carried forward" at page breaks, header on page 1 only, a page netting to zero,
day/month-ambiguous dates, December–January periods, cards in credit (`CR` closing), `*` markers, ±1 pt jitter,
transactions split by page breaks, price-like text at the end of descriptions, a fee column, a second currency
section. Real-statement variations added: uneven stretch, even pages shifted, 40% taller boxes, scrambled
headings, an injected OCR slip. Direct tests: figures in a detail line, learn → learn → undo → undo, a poisoned
profile. What they found and what changed:
- **Whole sections and pages silently missing** (the worst): a second account's rows after the first one's
  "Closing Balance" were never read, and with scrambled headings an Amex statement's KYD section vanished — the
  rest reconciled each time. Now a section heading resumes the table at its first dated line (its own summary box
  stays outside), and any dated line with an amount that no table read, from the top of the statement to the
  table's end (or anywhere, if nothing was read), holds it (`outside_dated_lines`). A statement whose only rows
  were "carried forward" lines had "reconciled" with nothing read.
- **"Balance carried forward" ended the table** and counted as the closing: it's a page-foot line now.
- **Ambiguous dates**: with no bank profile and no date past the 12th, every date could be read the wrong way
  round silently; now noted and held (`date_order_unproven`). Amounts like `5.20` no longer count as date evidence.
- **The first-row shortcut is gone**: a first row "known" from its column let a fee-column statement through with
  a fee missing. A first row with no opening balance is proven by the previous statement's closing at import
  (continuity), or the statement is held once.
- A card's `1,234.56 CR` summary balance is a credit (it had read as owed); fee columns ("Fee", "Charges",
  "Frais", "Comisión") are money out and two money-out columns on one line add up; a second date column needs only
  a quarter of the lines; two dates days apart on a line aren't mistaken for the period; lowercase words (`as of`)
  aren't margin lettering.
Measured: 0 of 10,400 generated statements wrong and passed through; 95–97% of provable ones read exactly (a
harder set again). Under scrambled headings or inflated boxes the dates and amounts of the owner's statements stay
exact; descriptions can lose a wrapped word or gain a reference number (reported, not failed).

**0.2.12: three upgrades from an outside review** (owner pasted another AI's proposals; built #1, #2, #4 as
amended, skipped most of #3 — a "date out of order" flag would hold real statements sorted by posting date or
cheque number, and the two-date check already existed).
1. **Description column guard.** A description's next line has to start in the description column (judged on the
   page's own transactions, so a page sitting to one side doesn't count against it): a notice centred on the page
   or a remark starting in the margin attaches to no transaction. The proposed line-spacing split wasn't built:
   many statements space every line alike. Shrinking inflated word boxes to the line pitch was tried and reverted:
   real card pages already have boxes taller than their line spacing and read correctly, so it changed real reads.
   Under 40% inflated boxes a card's wrapped word moves to the memo (`TOWN | Ref …`); nothing is lost, no amount
   changes. New real-statement variation: uneven vertical stretch — dates, amounts and descriptions all exact.
2. **Day/month order by plausibility** (`plausible_order`): when no date is past the 12th, the reading under which
   the statement is a statement — a period of at most 100 days (quarterly statements included) holding every
   line's date, ±7 days; or, with no period, dates spanning ≤ 100 days — is taken without a hold; when both or
   neither fit, it stays held as a guess. Generator: ambiguous-date statements went from 0% to ~70% read exactly
   (the rest fail on other features combined with them), none wrong.
4. **Account type settled by its lines** (`polarity_confirms`): a statement that doesn't say what account it is
   and prints no running balance isn't held when two or more lines' wording agrees with the type read and none
   disagrees (charges, interest, cash advances, ATM withdrawals, payments received, salary). Little visible effect:
   a new bank's first statement is held anyway, and a card read as a bank account is contradicted by its own lines,
   so it stays held.
Measured: 0 of 10,400 generated statements wrong and passed through; 95.5–96.2% of provable ones read exactly.

**Profiles for unknown banks (Approve):** approving a reconciled statement from a bank with no profile makes one
(`BankProfile::from_statement`): named and fingerprinted by what the letterhead says (`bank_identifiers`: a
proper name with Bank/Trust/Credit Union…, and the bank's web address — never an email domain or a merchant's
domain from a transaction line), with the layout it was read with and that account type verified. The account
is linked and renamed from `Bank …`; the bank's other held statements are re-read. A letterhead that names no
bank makes nothing ("set it up in Bank Profiles → New from Sample"). Fingerprints count triple only in page 1's
letterhead and footer: a page-1 transfer line saying "Credit Union" had made a statement CICSA's.

### 6.5 Currencies and exchange rates (0.2.13)

Many people in Cayman keep accounts back home. FinGraph knows 65 currencies (`money::CURRENCIES`: code, symbol,
name — codes that are also English words such as ALL, TOP, CUP, PEN, TRY, GEL, MAD, BOB are left out, or a
description's "TOP UP 20.00" would read as Tongan pa'anga). `Currency` is an index into that table, stored and
serialised as the ISO code, so existing ledgers needed no change.
- **Which currency a statement is in**: page 1's codes and symbols (`GBP`, `£`, `C$`, `J$`, `₱`…). KYD/USD as before;
  another currency wins when it's named more often than either. A page that names none (only `$`) is read as KYD
  and noted (held): a bare `$` can't be told apart. Accounts can be set to any currency in Preferences → Workspaces &
  Accounts (statements and rows follow, amounts are revalued). Workspaces, the header's display menu, the setup
  guide, the budget wizard and net-worth items take any currency.
- **Rates**: KYD↔USD stays on the peg, 1 KYD = 1.20 USD (no service quotes it reliably: Frankfurter 0.8372 per USD,
  fawazahmed0 0.8312). Every other currency goes through the US dollar at each day's rate, kept in `fx_daily`
  (units per USD, by day, with the source; schema v9): a transaction converts at the latest rate on or before its
  own day. A rate printed on a statement still wins. A currency with no rate yet converts one for one, and
  Preferences says so.
- **Downloads** (`rates.rs`, off by default — FinGraph's only network access, switched on in Preferences → Money →
  Exchange Rates): Frankfurter (`/v2/rates?base=USD&quotes=…&from=…&to=…`, central-bank and official rates, decades
  of history) first; the fawazahmed0 currency API (static daily files on jsDelivr, then its Cloudflare mirror, from
  2 March 2024) when Frankfurter is down. Only currency codes and dates are sent, via `curl`. What's fetched:
  each statement period not yet covered, and the days since the latest rate (`Db::rates_needed`), at start-up,
  after imports, when a currency is chosen, and on Update Now. Chosen after testing both live on 2026-10-30 (and
  ExchangeRate-API's free tier, which has no history); Frankfurter had one outage that morning. No self-hosted
  server option (owner's choice).
- Checked: `a_statement_in_pounds_converts_at_each_days_rate` (GBP statement → GBP account, rates asked for, each
  row at its own day's rate into KYD); `currencies_by_code_and_symbol`, `dated_rates_through_the_dollar`; the
  services' answers parsed offline (`both_answers_read_as_units_per_dollar`) and live (`live_rates`, ignored). Sample
  statements now read as INR, SGD, EUR, HKD + USD; the `$`-only ones are noted. The owner's ledger: every converted
  amount identical after migration and Rescan All.

### 6.6 Bank downloads: CSV, OFX, QFX (2026-10-31)

RBC gives CSV/OFX downloads, and a tester's RBC file was refused ("Only PDF statements can be imported").
`parser/exports.rs` reads them into the same `ScanResult` as a PDF (`MultiPassScanner::scan_file`
checks the file contents: `%PDF` → PDF scanner; `OFXHEADER`/`<OFX>` → OFX; anything else text-like → CSV),
so account matching, holds and dedup are unchanged. One section per account number + currency: RBC's CSV
puts every account in one file (`Account Type, Account Number, Transaction Date, Cheque Number,
Description 1, Description 2, CAD$, USD$`). CSV columns are found from their headings; amounts are taken as
signed for the account holder (money out negative, cards too); newest-first files are reversed; numeric
dates are day-first only if a day past the 12th proves it (otherwise a note holds the import). A Balance column
→ reconciled from it (row 1 unproven); none → Unverified (NoBalances hold, or the previous statement's closing
proves it). OFX gives the closing (`LEDGERBAL`) only. Kept copies are still `<sha>.pdf` whatever they hold.
The watched folder stays PDF-only (FinGraph's own CSV/OFX exports look like bank downloads); FinGraph's
CSV export is refused outright. Unverified until a real RBC download is tried.

Tested 2026-10-31 before commit: the ofxparse corpus (real anonymised bank/card OFX 1.x and 2.x files;
investment/sign-on-only files are refused), 14 awkward hand-made files, ledger behaviour, and the GUI. Found and
fixed: CDATA/entity values in OFX; two unnumbered accounts merged (group key now number + type + currency);
Amex-style cards printing purchases positive (no balances + mostly positive → flipped, with a note); Windows-1252
text; account/currency lines above the header; binary `.csv` errors. **Rescan All rule changed for every
statement:** an approved section that re-reads the same (same verdict, no new notes) stays approved; before,
NoBalances/OffLayout re-held it, which un-approved every download (and every approved no-balance PDF) on each
rescan. Checked on a copy of the real ledger: 33 files, 1,067 counted rows, identical before/after.

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
   read too: `pdfs_in` walks 8 folders down, at most 5,000 PDFs, and doesn't follow links, so a link
   to a parent can't loop. The walk runs off the main thread. The folder monitor sees new files at
   the top level; subfolders are walked again whenever the window comes back into focus). A watched
   file that can't be read or imported says so in a toast with **Import…**; it's remembered only when
   trying again can't change the answer (a missing tool, a database error and a needed decision are
   offered again). Continue needs at least one PDF (or statements already).
5. **Read Them** — reads every file not read before (by path, size and time) with a progress bar
   and one row per file (bank, accounts, period, transactions, Adds up / Off by / Couldn't read),
   then: statements · accounts · transactions and the date span; money in and out with a monthly
   average (transfers between your own accounts left out); statements that didn't add up and how to
   fix them (Scan Pass Report → Convert); months covered per account (● ◐ ○) and what to do about
   gaps; Read Again. Leaving the step mid-read and coming back shows the same read's progress (Continue
   waits for it). Files read here are remembered so the watcher doesn't scan them again, and the
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

- `cargo deb -p fingraph` → `target/debian/fingraph_0.2.14-1_amd64.deb` (binary, desktop entry with
  PDF "Open With", icon, README; depends on poppler-utils, GTK 4, libadwaita; recommends
  tesseract-ocr for picture-only statements and curl for exchange-rate downloads).
- Install: `sudo apt install ~/Projects/fingraph-rs/target/debian/fingraph_0.2.14-1_amd64.deb`. The
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

- `cargo test --workspace`: 122 tests (0.2.14 whole-app review: `tests/review_regressions.rs` and `tests/ledger_regressions.rs` pin every reproduced finding; the watched-folder walk skips links and finds subfolders; the watched-folder walk skips links and finds subfolders; 0.2.7: a bill picked by its due month lands on the next one; 0.2.3–0.2.4 added the real CNB layout and OCR, multi-line
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
- A bug in any action, toast button, dialog response, page refresh or background import shows "Something
  went wrong" and writes `crash.log` instead of closing the app (`guard`, `shielded`, `spawn_guarded`);
  "this change isn't the user's" flags are reset by a drop guard (`Quiet`), so a panic can't leave them on.
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
| Overview KPI cards (0.2.8) | Owner: "2 rows of 5 … net cash flow, annually, monthly and fortnightly", with a toggle on each. The date-range Net Cash Flow card is replaced by three: **Year / Month / Fortnight**, each with its own menu (saved as `overview_flow_year|month|fortnight`): **Average** = last 12 months of statements (fewer if that's all there is) scaled by days (365, 365/12, 14), **Range** = the picked date range scaled the same way, **Actual** = this year so far, the last month the statements fully cover, and the last 14 days of statements. Added **Left to Spend This Month** (same sum as the Budget page's Left: funded − spent in budgeted categories, `budget::left_this_month`), **Next Big Bill** (soonest fund, amount, set aside, `budget::next_big_bill`) and **Cash Runway** (latest balance of visible non-card/loan accounts ÷ average monthly spending over the last 12 months). Layout: a 5-column homogeneous grid; the menus sit under the numbers so the row fits beside the sidebar |
| Transactions freeze + packaging (0.2.15) | 0.2.14 routed the Transactions filter dropdowns through `App::refresh`; the page's own dropdown rebuild then queued another refresh forever at HIGH_IDLE, so the window never painted when opened on Transactions. The handlers now skip while the page refreshes. Packaged as .deb, .rpm (`cargo generate-rpm`, requires the file /usr/bin/pdftotext and libGLESv2) and Arch `packaging/aur` PKGBUILD; installs and starts on Debian 13, Ubuntu 26.04, Fedora 42/43, Tumbleweed, Arch. Public GitHub copy is the orphan branch `public` (one commit, no history, no PRD, test data scrubbed of the owner's accounts and pay) |
| Whole-app review 0.2.14 (owner: "design and execute a comprehensive review … fix them all") | Five independent reviews (engine, ledger, money maths, app, security) with every high finding reproduced by a test before fixing; the rows below are the outcome. |
| Whole-app review 0.2.14: statement engine | Four ways a wrong read still said Reconciled are now held: a balance blamed as misprinted (two rows had traded amounts), a sign heal standing in for a lost row (no heal across an unread dated line; that note no longer needs a profile), card/loan detected from words inside other words ("advisable", "Principal Office"; whole words now), a currency taken from a name ("Dear Ron" → RON; codes count only outside transaction lines, in capitals, and any non-KYD/USD currency with no profile is noted). Also: printed debit/credit totals checked against the rows, recovered lines noted, two-digit years pivot (97 → 1997), ASCII digits only (full-width digits panicked), printed amounts capped below 10 trillion, checked FX arithmetic. OCR: time limits on every tool, pages rendered one at a time capped at 4200 px (a 5.7 KB PDF used to ask for ~17 GB), 0700 scratch folders under XDG_RUNTIME_DIR, stale ones swept. Layout samples scale every amount by one random factor and mask far dates and short words. |
| Whole-app review 0.2.14: ledger | Statement copies live in `statements/` beside the ledger (a scratch ledger used to work on the real copies — it moved them once during the review, put back within a minute); Move/Erase only touch `<sha256>.pdf` files, a non-empty target gets a `FinGraph Statements` subfolder. Rescan matches sections to accounts by number+currency, not position, and runs in one savepoint with its restore (an error used to delete the statement and the edits). Rows a held copy owns move to a clean copy; deleting one of two overlapping statements hands shared rows to the other (`shared_rows`, schema v10). Memo edits, deleted rows and unlinked transfers survive rescans; workspace/account ids never reused; each section uses its account's workspace currency and rules; failed auto-backups stop destructive actions; rates sanity-checked (no future days, 1e-6..1e7 per USD, ≤50% from the nearest stored rate); data folder 0700, ledger/backups/copies 0600. |
| Whole-app review 0.2.14: follow-ups | `TxFilter.by_account` picks rows by the account's own workspace: Net Worth and the balance forecast use it, so a business-flagged charge stays in its account's figures. Overview names currencies left out of totals for want of a rate. The watcher forgets files that no longer exist. |
| Whole-app review 0.2.14: desktop app | Display currency picked in Preferences is saved and redraws the pages (it was neither when the currency wasn't in the header list yet). The date range no longer opens the Custom dialog by itself after an import; ✎ beside it changes the custom dates, and dates it can't read say so. Overview's reconciliation tile counts statements waiting for review ("83/84 · 1 waiting for review"). Watched folder: read and import errors are toasted with Import…, subfolders are re-walked on focus, links aren't followed, the walk is off the main thread. Saves that fail say why, and "closed"/"removed"-style toasts only follow a save that worked; typed-but-unapplied payee/tag/memo is kept when the editor closes. Panics in actions, dialogs and imports are caught (see §11). Getting Started's Read Them step can be left and re-entered mid-read. Rescan All's warning follows "Rescan keeps my changes", and Rescan All is off while one runs. Consolidated: the label follows the ticks and the last workspace can't be unticked. The category drill-down's Back button has its label and always uses the current data. Charts that open something on click can be used from the keyboard (Tab, ←/→, Enter). Toasts say Categorize (not Triage); About, Preferences → Data and the package description no longer claim nothing is ever sent; `.deb` recommends curl |
| Layout learning (0.2.9) | Owner: make sure it learns new statements out of the box, e.g. if Scotiabank changes its layout. It read unknown banks already, but renamed column headings collapsed a chequing read (3 of 28 rows) and a rebranded letterhead was taken for CNB; the only fix was editing Bank Profiles. Now: page-1 fingerprint, re-reads proven by the balances (including columns from the amounts alone), learn-on-Approve with undo, and a hold when a statement looks different from the account's last one (§6.4) |
| Whole-app review 0.2.14: money maths | Refunds filed under a spending category net against it (by category, floored at 0) and lower expenses, not raise income; "vs prior period" is the calendar one before (Sep vs Aug, a year vs the year before, YTD vs the same dates last year); monthly/quarterly/yearly bills step by calendar month (February keeps bills paid on the 1st or the 31st); subscription yearly cost counts cycles (CI$10 monthly = CI$120); a charge flagged to a business doesn't bring its personal account's balance or forecast into the business view, and the account's export keeps it; split charges count once for double-charge and renewal checks; APR counts only interest, over the statements actually there (max 3); history converts at the posting day's rate and a currency with no rate yet is left out of totals (`missing_rates`), never counted 1:1 |
---

## 12a. Confidence in statement reading (assessed 2026-10-30, 0.2.11; rows updated for 0.2.12 and 0.2.14)

| Question | Confidence | Basis |
|---|---|---|
| Wrong figures counted without anyone seeing them | Very unlikely | Independent nets: every row proven by the balances (coverage, not just totals); each statement must open where the account's last one closed; dated amount lines no table read hold it; ambiguous day/month and card-vs-bank wording are flagged; a new bank or layout is always held once |
| Amounts right when a statement says Reconciled | ~97%+ | 0 of 16,400 generated statements (seeds 1, 77, 2026, 5150) wrong and passed through after the 0.2.14 review — which still found four new ways a wrong read could look proven, all now held; 0 of 10,400 after 0.2.11's fixes; every distortion of the owner's 12 statements keeps dates and amounts exact |
| A new bank's first statement reads right | ~60–80% (estimate) | Generator 95–97% of provable statements, but it was tuned alongside the engine; unfamiliar real files have run nearer half to two-thirds. Stricter rules mean more correct reads wait for one approval |
| Dates right | Medium-good | Transaction/posting order, ambiguous orders settled by the period (or held), rollovers tested — but balances can't prove a date |
| Descriptions right | Probably | Not checked by anything; off-column lines can't attach (0.2.12); under distorted layouts a wrapped word can move to the memo or a reference number prefix the description |
| The owner's Scotiabank statements | Very high | Every check passes on all 41 sections; they chain month to month |

Why not higher: every testing round so far found a new way for a wrong read to look proven (false Reconciled,
a lost first row, a netted line, a silently wrong year, missing sections). All are fixed, but the generator only
covers failure modes already thought of. What would raise it most: one real PDF per account type from each bank
in use (Butterfield, RBC, CNB, the Credit Union) — none from RBC or Butterfield has been seen.

## 13. Outstanding / known limits

- A statement that only prints `$` can't say whether it's KYD, USD, CAD…: it's held as KYD with a note, and the
  account's currency is set by hand once. Currencies outside the 65 (or without a Frankfurter/fallback rate)
  convert one for one.
- **Three of five banks unverified on real PDFs** (Butterfield, RBC, CICSA): follow §6.3. Until then
  every statement from them is held for review on import; the first approval of each teaches its profile (§6.4).
- Not read (out of scope for KYD/USD Cayman statements): decimal-comma amounts (`15.320,00 €`, `10 750,00 $`),
  Dutch/French month names (`1 mei`, `1 janvier`); other currencies (HKD, INR, EUR…) are read but filed as the
  account's KYD/USD. An unknown bank's approved statement teaches nothing until it has a profile (New from Sample).
- Layout learning (§6.4) is proven on simulated changes to real Scotiabank statements, not on a real
  layout change yet. Columns from the amounts alone need two or more amounts per column; a statement with
  no balance column and no headings the scanner knows still needs the inspector.
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
