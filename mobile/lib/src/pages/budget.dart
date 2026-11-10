import 'package:flutter/material.dart';

import '../data.dart';
import '../widgets.dart';

/// Each category's month: budget, spent, left.
class BudgetPage extends StatelessWidget {
  const BudgetPage({super.key, required this.app});
  final AppState app;

  @override
  Widget build(BuildContext context) {
    return ViewLoader(
      app: app,
      name: 'budget',
      builder: (context, l) {
        final d = l.data;
        final sym = d['symbol'] as String? ?? r'$';
        final cats = (d['categories'] as List?) ?? [];
        final t = Theme.of(context);
        return ListView(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 32),
          children: [
            MonthBar(app: app, workspaces: (d['workspaces'] as List?) ?? []),
            OfflineNote(l),
            Padding(
              padding: const EdgeInsets.fromLTRB(4, 8, 4, 8),
              child: Text('${money(d['spent'], sym)} spent of ${money(d['budgeted'], sym)} · ${money(d['left'], sym)} left', style: t.textTheme.titleMedium),
            ),
            if (cats.isEmpty)
              const Message(icon: Icons.pie_chart_outline, title: 'No budget for this month', text: 'Set one up on the desktop (Budget).')
            else
              Card(
                child: Column(children: [
                  for (final c in cats)
                    Padding(
                      padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
                      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                        Row(children: [
                          Expanded(child: Text(c['name'] as String, style: t.textTheme.titleSmall)),
                          Text(
                            c['state'] == 'unbudgeted'
                                ? '${money(c['spent'], sym)} · no budget'
                                : number(c['left']) < 0
                                    ? '${money(-number(c['left']), sym)} over'
                                    : '${money(c['left'], sym)} left',
                            style: t.textTheme.bodyMedium?.copyWith(color: c['state'] == 'over' ? t.colorScheme.error : null),
                          ),
                        ]),
                        const SizedBox(height: 8),
                        Meter(spent: number(c['spent']), budget: number(c['budget']), state: c['state'] as String? ?? 'ok'),
                        const SizedBox(height: 4),
                        if (c['state'] != 'unbudgeted') Text('${money(c['spent'], sym)} of ${money(c['budget'], sym)}', style: t.textTheme.bodySmall),
                      ]),
                    ),
                ]),
              ),
          ],
        );
      },
    );
  }
}
