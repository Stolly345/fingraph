import 'package:flutter/material.dart';

import '../data.dart';
import '../widgets.dart';

/// The month at a glance: what's left, what's over, what to do next, the accounts.
class SummaryPage extends StatelessWidget {
  const SummaryPage({super.key, required this.app});
  final AppState app;

  @override
  Widget build(BuildContext context) {
    return ViewLoader(
      app: app,
      name: 'summary',
      builder: (context, l) {
        final d = l.data;
        final sym = d['symbol'] as String? ?? r'$';
        final t = Theme.of(context);
        final left = number(d['left']);
        final budgeted = number(d['budgeted']);
        final spent = number(d['spent']);
        final alerts = (d['alerts'] as List?) ?? [];
        final steps = (d['next_steps'] as List?) ?? [];
        final accounts = (d['accounts'] as List?) ?? [];
        return ListView(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 32),
          children: [
            MonthBar(app: app, workspaces: (d['workspaces'] as List?) ?? []),
            OfflineNote(l),
            const SizedBox(height: 8),
            Card(
              child: Padding(
                padding: const EdgeInsets.all(20),
                child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                  Text(budgeted > 0 ? (left >= 0 ? 'Left to spend' : 'Over budget by') : 'Spent', style: t.textTheme.labelLarge?.copyWith(color: t.colorScheme.onSurfaceVariant)),
                  const SizedBox(height: 4),
                  Text(money(budgeted > 0 ? left.abs() : spent, sym),
                      style: t.textTheme.displaySmall?.copyWith(fontWeight: FontWeight.w600, color: left < 0 && budgeted > 0 ? t.colorScheme.error : null)),
                  if (budgeted > 0) ...[
                    const SizedBox(height: 16),
                    Meter(spent: spent, budget: budgeted, state: left < 0 ? 'over' : 'ok'),
                    const SizedBox(height: 8),
                    Text('${money(spent, sym)} of ${money(budgeted, sym)} spent', style: t.textTheme.bodySmall),
                  ],
                  const SizedBox(height: 12),
                  Row(children: [
                    _Flow(label: 'Money in', value: money(d['income'], sym), icon: Icons.south_west),
                    const SizedBox(width: 24),
                    _Flow(label: 'Money out', value: money(d['spending'], sym), icon: Icons.north_east),
                  ]),
                  if (d['coverage'] is String && (d['coverage'] as String).isNotEmpty) ...[
                    const SizedBox(height: 12),
                    Text(d['coverage'] as String, style: t.textTheme.bodySmall?.copyWith(color: t.colorScheme.onSurfaceVariant)),
                  ],
                ]),
              ),
            ),
            if (alerts.isNotEmpty) ...[
              const SectionTitle('Watch'),
              Card(
                child: Column(children: [
                  for (final a in alerts)
                    ListTile(
                      leading: Icon(a['state'] == 'over' ? Icons.error_outline : Icons.warning_amber, color: a['state'] == 'over' ? t.colorScheme.error : Colors.orange.shade700),
                      title: Text(a['name'] as String),
                      subtitle: Text(a['state'] == 'over'
                          ? '${money(number(a['spent']) - number(a['budget']), sym)} over its ${money(a['budget'], sym)}'
                          : '${money(a['left'], sym)} left of ${money(a['budget'], sym)}'),
                    ),
                ]),
              ),
            ],
            if (steps.isNotEmpty) ...[
              const SectionTitle('Next steps'),
              Card(
                child: Column(children: [
                  for (final s in steps)
                    ListTile(
                      leading: Icon(_stepIcon(s['kind'] as String?)),
                      title: Text(s['title'] as String),
                      subtitle: s['detail'] == null ? null : Text(s['detail'] as String),
                    ),
                ]),
              ),
            ],
            if (accounts.isNotEmpty) ...[
              const SectionTitle('Accounts'),
              Card(child: Column(children: [for (final a in accounts) AccountTile(a as Map<String, dynamic>)])),
            ],
          ],
        );
      },
    );
  }

  static IconData _stepIcon(String? kind) => switch (kind) {
        'review_statements' => Icons.fact_check_outlined,
        'categorize' => Icons.label_outline,
        'missed_bill' => Icons.event_busy_outlined,
        'statement_due' => Icons.description_outlined,
        'low_balance' => Icons.trending_down,
        _ => Icons.arrow_forward,
      };
}

class _Flow extends StatelessWidget {
  const _Flow({required this.label, required this.value, required this.icon});
  final String label;
  final String value;
  final IconData icon;
  @override
  Widget build(BuildContext context) {
    final t = Theme.of(context);
    return Row(children: [
      Icon(icon, size: 16, color: t.colorScheme.onSurfaceVariant),
      const SizedBox(width: 6),
      Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Text(label, style: t.textTheme.labelSmall?.copyWith(color: t.colorScheme.onSurfaceVariant)),
        Text(value, style: t.textTheme.titleSmall),
      ]),
    ]);
  }
}

class AccountTile extends StatelessWidget {
  const AccountTile(this.a, {super.key});
  final Map<String, dynamic> a;
  @override
  Widget build(BuildContext context) {
    final bal = a['balance'];
    final cur = a['currency'] as String? ?? '';
    return ListTile(
      leading: Icon(switch (a['type']) { 'CreditCard' => Icons.credit_card, 'Loan' => Icons.account_balance, 'Savings' => Icons.savings_outlined, _ => Icons.account_balance_wallet_outlined }),
      title: Text(a['name'] as String),
      subtitle: Text(a['as_of'] == null ? 'No statement yet' : 'As of ${shortDay(a['as_of'] as String)}'),
      trailing: Text(bal == null ? '—' : '${money(bal, '')} $cur', style: Theme.of(context).textTheme.titleSmall),
    );
  }
}
