# FinGraph: Product Requirements

Last updated: 8 November 2026 (version 0.2.22, with the interface redesign unreleased)

## 1. What FinGraph is

A local-first desktop ledger and budget for people banking in the Cayman Islands. Cayman banks give
no live feeds to budgeting apps, so FinGraph works from the PDF statements you download each month.
It reads them, proves every line against the statement's own balances, sorts the transactions, and
turns them into a budget, a forecast and a net-worth picture. The ledger never leaves the computer.

The README describes everything FinGraph does today, page by page. This document records **why** it
works the way it does, what it must keep doing, and what comes next.

## 2. Who it's for

- **The household budget-keeper** in Cayman with accounts at one or more of Butterfield,
  Scotiabank, Cayman National, RBC and the CICSA Credit Union, in KYD and USD.
- **Small business and rental owners** who keep personal and business money apart in workspaces,
  but budget at home as one household.
- Comfortable downloading statements once a month; not interested in bookkeeping jargon.

## 3. Principles

1. **Proven numbers.** A statement counts only once it balances to the cent, or once you approve it.
2. **Your answers win.** Rules you make beat the directory, which beats keyword guesses. A
   transaction you confirmed by hand is never changed by anything automatic.
3. **Local and private.** Nothing about your money goes online. The only network use is the daily
   update check and, if switched on, exchange rates (currency codes and dates only).
4. **Plain language.** Every card explains itself (ⓘ). Labels say what something does, not what
   it's called in accounting.
5. **Something to act on.** Every analysis should end in something you can do: a card to answer, a
   bill to cancel, a transfer to make.

## 4. The monthly loop

1. Download last month's statements; FinGraph reads and reconciles them.
2. Answer **To Review** (one card per merchant).
3. Run **Monthly Close**: check coverage, sort what's new, settle the budget, tick off bills, close.
4. Take the result away: the **Report Card** for the month, and an updated **Budget Plan**.
5. Between statements, **Outlook** shows where balances are heading.

## 5. Feature: the budget you walk away with (shipped in 0.2.19)

### Problem

Setting a budget in FinGraph produced good numbers on screen, but nothing to act on away from the
app. A table of categories and amounts is read once and forgotten, and it goes out of date with the
next statement. A budget only works if it says what to do on payday, what's due when, and how much
can be spent this week, and if each month is then checked against it.

### Requirements

**R1. Accounts have roles.** The budget setup has a sixth step, *Where Your Money Goes*: which
account pay lands in, which pays the fixed bills, which is used for everyday spending (often a
card), where the big-bill money is kept, and where savings go. Big-bill money and savings can also
go to "a savings account outside FinGraph". FinGraph guesses first: pay goes to the account with the
most income, bills to the account most regular-bill money leaves from, and spending to the account
with the most spending charges.

**R2. The Budget Plan (PDF, save or print).** On the Budget page, under **Budget Plan**, and offered
straight after saving the budget. Letter size, dated, and naming the newest statement it's based on.

- *Page 1, for the fridge:*
  - Four figures: take-home a payday, spending a month, big bills a month, saving a month.
  - **Every Payday:** a tick-box checklist of what to move from the pay account to each other
    account, and what to leave, per payday. Pots that go to the same account share one line. What
    is left over each payday is shown, or a warning if the plan spends more than comes in.
  - **Bills Coming Up:** the regular bills due in the next 31 days and big bills due in the next 3
    months, with the account each comes from. A regular bill is a subscription or a charge in a
    fixed-expense category, so a fortnightly lunch at the same place doesn't count. Cancelled
    services, and bills already set aside for as big bills, are left out.
  - **Weekly Spending Limits:** each variable category's budget a week and a month, against what it
    usually costs. Red marks a category whose usual cost is more than its budget.
  - **Savings:** where the monthly saving leads by year end, in a year and in five years.
- *Detail pages:*
  - Every category: fixed or variable, budget, usual cost and the room between them.
  - Every big bill: cost, how often, the monthly share, next due date, and what should be saved by
    now. If the big-bill account is in FinGraph, its latest balance is checked against that figure.
  - The arithmetic: take-home minus fixed, variable, big bills and loans gives what's left to save.
    It also shows the gap to the savings goal.
  - Cancelled subscriptions and what that saves a year.

**R3. The Report Card (PDF, save or print).** For any month, from **Monthly Close** (its last step)
or the Budget Plan menu (the last full month):

- Saved, spent and income against the plan.
- **What to Do Next:**
  - the categories that went over,
  - the biggest spending with no budget,
  - a big-bill account behind what it should hold,
  - savings under the goal.
- Category by category: budget, spent, left.
- Spending without a budget.
- The month's money, plan against actual.

**R4. Calendar file (.ics).** The next 12 months, as all-day events:

- **Each payday:** the checklist with amounts, and a reminder at 9 a.m. that day.
- **Each regular bill:** a reminder three days before.
- **Each big bill:** a reminder two weeks before.

Events have stable ids, so importing a newer file updates events rather than doubling them.

**R5. Cancelling.**

- **Where:** in budget setup (bills and subscriptions) and on the Subscriptions page (click the
  amount).
- **What it does:** a cancelled service stops counting in subscription totals and the budget, and
  isn't suggested again.
- **Every spelling:** cancelling covers all the ways the bank spelled the same service.
- **If it charges again:** a charge after the day it was cancelled brings it back.

### Acceptance (checked for 0.2.19)

- The plan and report card render from a real ledger with no cut-off or missing characters. Page 1
  holds the checklist and the bills.
- Unit tests cover the checklist pots and merging, the weekly figures, the report card sums, the
  calendar (counts, reminders, folding at 75 bytes) and cancel-then-charge-again.

## 6. Feature: budgeting methods (shipped in 0.2.21)

### Problem

The budget started from what you usually spend, and that's all. Many people know a method by name
(50/30/20 and the rest) and want their budget built from it. FinGraph already showed these rules as
a comparison card on the Budget page, but only to grade a month after the fact.

### Requirements

**M1. Choose a method in setup** (step 4 of 7, after the big bills). The methods are 50/30/20,
70/20/10, 80/20 (pay yourself first), zero-based, and my own numbers. The step shows, with your
figures, what you usually spend on needs and wants, what the method allows, and what doesn't fit.

**M2. Needs and wants.** Each category is marked Need or Want on the spending step. Defaults:
housing, utilities, insurance, groceries, transport, health, family, education, fees, repairs and
professional services are needs; the rest are wants. Needs are not the same as fixed: groceries are
a need but vary. Big yearly bills and loan minimums count as needs.

**M3. The method fills the budget.** Leaving the method step fills the spending, savings and debt
figures once per choice; **Fill Again by My Method** redoes it. The rules:

- **Needs** stay at what they usually cost.
- **Wants** are scaled down to the room the method leaves, rounded down to CI$5. They are never
  scaled up.
- **Debt:** 70/20/10's 10% goes to debt, capped at what's owed, and to savings once nothing is owed.
- **Savings** get whatever isn't spent or paid off.

**M4. The pension counts as saving.** The method's percentages are of take-home plus the pension
contribution (deductions named "pension"), and the pension counts toward the savings share. Only
the rest has to be moved by hand.

**M5. Zero-based means every dollar has a job.** The savings step shows what's still unassigned and
offers **Put the Rest in Savings**. The budget can't be saved until the unassigned amount is zero.

**M6. Debt.**

- A new "pay down debt per month" amount sits on top of the minimums.
- A new account role says where that money goes. Its default is the card or loan with the most owed.
- The payday checklist moves the money there.
- The Budget Plan says what's owed and roughly when it will be cleared.

**M7. The plan and report card follow the method.**

- **Budget Plan:**
  - The title names the method.
  - The tiles show each share against its target.
  - When needs crowd out wants, page 1 says so and names the biggest needs to bring down.
  - 80/20 shows one weekly allowance instead of a limit per category.
  - The detail pages add an "Against <method>" table and show need/want on every category.
- **Report Card:** grades the month's needs, wants and saving (pension included) as shares of what
  came in.

## 7. Feature: the interface redesign (finished on the `interface-redesign` branch, not yet released)

### Problem

FinGraph had the functions but read as a long page of analysis: fourteen destinations, a Budget page
whose categories started below the fold, and an Overview with eight figures before its first chart.
People need one job per screen, and categories they recognise at a glance.

### Decisions

- **FinGraph opens on Budget, always.** There is no Home page and the last-used page isn't remembered.
- **The sidebar is Budget, Transactions, Reports, Statements (with Combined Statement under it), Summary,
  then a collapsible Accounts list.**
- **Budget follows the method chosen at setup.** It is zero-based only for those who choose zero-based.
- **Summary is Overview and Net Worth together.**

- **A light/dark button sits beside the main menu.** It sets the same Appearance preference as
  Preferences, so the two always agree.

### Status

- [x] **Step 1. Category identity and polish.** Every category has a symbolic icon and a named colour,
  with defaults for the built-in ones (sub-categories wear their parent's) and a chooser under Rules &
  Categories → Icons and Colours. Charts colour a category the same everywhere. ⓘ buttons are quiet
  until a card is hovered, and stay visible enough for touchscreens. The demo household now pays a home
  loan and has its routine bank charges already sorted.
- [x] **Step 2. New sidebar, opening on Budget.** Page shortcuts are Ctrl+1 to Ctrl+6. Both folds are
  remembered. Reports is one row with a strip of tabs.
- [x] **Step 3. Budget rebuild.** Budget opens on the newest month with statements and leads with one
  number, chosen by the method: *Left to Spend* (My Own Numbers, 50/30/20, 70/20/10), *a weekly spending
  figure* (80/20) or *Still Needs a Job* (Zero-Based, with a button that gives the rest to savings).
  Categories sit in one table per group (Bills and Everyday; Needs and Wants; Spending), with tile, bar,
  planned, spent and left; everything else about a category is on its ⋯ menu. 50/30/20 and 70/20/10 show
  each part of the split against its target, and 70/20/10 adds when the debts clear. 80/20 has no category
  table: a week-by-week look back at the month, the payday move as a checklist item, and bills
  apart. Its weekly figure is the Budget Plan PDF's definition (variable budgets, per week), so screen and
  paper agree. Review and the "proven to the cent" line moved here. Your Plan, Fixed vs Variable, Rules of
  Thumb and the money-flow chart moved to Reports. The method's rules live in `fingraph-core` and are tested.
- [x] **Step 4. Summary.** Net worth leads, then the Overview figures and charts for the chosen date range.
  Accounts, other assets and debts, goals and debt payoff are kept, folded under the charts (the fold is
  remembered). Left to Spend and the Review banner are no longer repeated here; they live on Budget.
- [x] **Step 5. Reports.** One sidebar row and a tab strip: Cash Flow, Trends, Where It Went, Plan, Outlook,
  Subscriptions, What Changed and Export. Cash Flow and Trends are new: one large chart and a ranked
  breakdown over the twelve months ending with the newest statement, with stacked bars by category for Trends.
- [x] **Step 6. Finish.** Getting Started, card help, the README and its screenshots all describe the new
  layout. Old `last_page` values in saved preferences are ignored.

## 8. Known issues and next steps

Left out of the interface redesign, in the order they'd help most: **manual transaction entry** (which
would let 80/20's weekly view cover the current week, since statements only arrive after a month ends),
multi-step undo, more themes, and customisable Summary widgets.

1. **Twice-a-month pay ignores the payday you type.** It assumes the 15th and the last day, so a
   payday on the 12th gives 12th, 15th, 30th. Fix: step from the typed day.
2. **Gross and take-home mixed up.** If the detected deposit (take-home) is typed as gross and
   deductions are added, they are taken off twice. The setup should warn when gross minus
   deductions comes out below the deposits it can see.
3. **Big bills made from cancelled services** (an earlier Yahoo Mail or Google entry, say) stay in
   the budget until removed in setup. The setup could offer to remove them when the service is
   cancelled.
4. **Report card history.** A page comparing closed months side by side (saved, spent, over).
5. **Wants cut to nothing.** When needs take most of the money (a large repairs budget, a big
   yearly landscaping bill), a method can leave wants close to zero. The plan says so, but setup could
   also suggest which needs to question first.
6. **Todoist.** Optional: send payday checklists and bill reminders as tasks, for people who live in
   Todoist rather than a calendar.
7. **Several pay sources.** Two earners or a second job: more than one pay line, each with its own
   account and cycle.
