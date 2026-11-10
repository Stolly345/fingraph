import 'package:flutter/material.dart';

import '../connection.dart';
import '../data.dart';
import '../lock.dart';
import '../widgets.dart';
import 'summary.dart' show AccountTile;

/// Bills, subscriptions, accounts and settings.
class MorePage extends StatelessWidget {
  const MorePage({super.key, required this.app});
  final AppState app;

  @override
  Widget build(BuildContext context) {
    void open(String title, Widget body) => Navigator.push(context, MaterialPageRoute<void>(builder: (_) => Scaffold(appBar: AppBar(title: Text(title)), body: body)));
    return ListView(padding: const EdgeInsets.symmetric(vertical: 8), children: [
      ListTile(leading: const Icon(Icons.event_repeat), title: const Text('Bills'), subtitle: const Text('What this month should charge, and what did'), onTap: () => open('Bills', BillsPage(app: app))),
      ListTile(leading: const Icon(Icons.subscriptions_outlined), title: const Text('Subscriptions'), subtitle: const Text('What they cost in a year'), onTap: () => open('Subscriptions', SubscriptionsPage(app: app))),
      ListTile(leading: const Icon(Icons.account_balance_outlined), title: const Text('Accounts'), subtitle: const Text('Balances from the latest statements'), onTap: () => open('Accounts', AccountsPage(app: app))),
      const Divider(),
      ListTile(leading: const Icon(Icons.settings_outlined), title: const Text('Settings'), subtitle: const Text('Server, app lock, privacy'), onTap: () => open('Settings', SettingsPage(app: app))),
    ]);
  }
}

class BillsPage extends StatelessWidget {
  const BillsPage({super.key, required this.app});
  final AppState app;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: app,
      builder: (context, _) => ViewLoader(
        app: app,
        name: 'bills',
        builder: (context, l) {
          final d = l.data;
          final sym = d['symbol'] as String? ?? r'$';
          final items = (d['items'] as List?) ?? [];
          final t = Theme.of(context);
          (IconData, Color, String) look(Map b) => switch (b['state']) {
                'paid' => (Icons.check_circle, Colors.green.shade700, 'Paid ${((b['paid_on'] as List?) ?? []).map((e) => shortDay('$e')).join(', ')}'),
                'missed' => (Icons.event_busy, t.colorScheme.error, 'Not charged, due ${shortDay(b['due'] as String)}'),
                'new' => (Icons.fiber_new_outlined, t.colorScheme.primary, 'New this month'),
                _ => (Icons.schedule, t.colorScheme.onSurfaceVariant, 'Due ${shortDay(b['due'] as String)} · statement not in yet'),
              };
          return ListView(padding: const EdgeInsets.fromLTRB(16, 0, 16, 32), children: [
            MonthBar(app: app, workspaces: (d['workspaces'] as List?) ?? []),
            OfflineNote(l),
            Padding(padding: const EdgeInsets.fromLTRB(4, 8, 4, 8), child: Text('${items.length} regular bills · about ${money(d['total'], sym)}', style: t.textTheme.titleMedium)),
            if (items.isEmpty) const Message(icon: Icons.event_repeat, title: 'No regular bills found for this month'),
            if (items.isNotEmpty)
              Card(
                child: Column(children: [
                  for (final b in items)
                    () {
                      final (icon, color, text) = look(b as Map);
                      return ListTile(leading: Icon(icon, color: color), title: Text(b['payee'] as String), subtitle: Text('$text · ${b['account']}'), trailing: Text(money(b['amount'], sym)));
                    }(),
                ]),
              ),
          ]);
        },
      ),
    );
  }
}

class SubscriptionsPage extends StatelessWidget {
  const SubscriptionsPage({super.key, required this.app});
  final AppState app;

  @override
  Widget build(BuildContext context) {
    return ViewLoader(
      app: app,
      name: 'subscriptions',
      builder: (context, l) {
        final d = l.data;
        final sym = d['symbol'] as String? ?? r'$';
        final items = (d['items'] as List?) ?? [];
        final t = Theme.of(context);
        return ListView(padding: const EdgeInsets.fromLTRB(16, 8, 16, 32), children: [
          OfflineNote(l),
          Card(
            child: Padding(
              padding: const EdgeInsets.all(20),
              child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                Text('Subscriptions cost', style: t.textTheme.labelLarge?.copyWith(color: t.colorScheme.onSurfaceVariant)),
                Text('${money(d['yearly'], sym)} a year', style: t.textTheme.headlineMedium?.copyWith(fontWeight: FontWeight.w600)),
                Text('about ${money(d['monthly'], sym)} a month', style: t.textTheme.bodyMedium),
              ]),
            ),
          ),
          const SizedBox(height: 8),
          if (items.isEmpty) const Message(icon: Icons.subscriptions_outlined, title: 'No subscriptions marked yet', text: 'Mark them on the desktop and they show here.'),
          if (items.isNotEmpty)
            Card(
              child: Column(children: [
                for (final s in items)
                  ListTile(
                    enabled: s['active'] == true,
                    title: Text(s['payee'] as String),
                    subtitle: Text(s['active'] == true ? '${s['cadence']} · next ${shortDay(s['next'] as String)}' : 'Stopped · last ${shortDay(s['last'] as String)}'),
                    trailing: Column(mainAxisAlignment: MainAxisAlignment.center, crossAxisAlignment: CrossAxisAlignment.end, children: [
                      Text(money(s['amount'], sym), style: t.textTheme.titleSmall),
                      if (s['active'] == true) Text('${money(s['yearly'], sym)}/yr', style: t.textTheme.bodySmall),
                    ]),
                  ),
              ]),
            ),
        ]);
      },
    );
  }
}

class AccountsPage extends StatelessWidget {
  const AccountsPage({super.key, required this.app});
  final AppState app;

  @override
  Widget build(BuildContext context) {
    return ViewLoader(
      app: app,
      name: 'accounts',
      builder: (context, l) {
        final items = (l.data['accounts'] as List?) ?? [];
        return ListView(padding: const EdgeInsets.fromLTRB(16, 8, 16, 32), children: [
          OfflineNote(l),
          if (items.isEmpty) const Message(icon: Icons.account_balance_outlined, title: 'No accounts yet'),
          if (items.isNotEmpty) Card(child: Column(children: [for (final a in items) AccountTile(a as Map<String, dynamic>)])),
        ]);
      },
    );
  }
}

class SettingsPage extends StatefulWidget {
  const SettingsPage({super.key, required this.app});
  final AppState app;

  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  Map<String, dynamic>? _status;
  String? _error;

  @override
  void initState() {
    super.initState();
    widget.app.server?.me().then((m) {
      if (mounted) setState(() => _status = m['server'] as Map<String, dynamic>?);
    }).catchError((Object e) {
      if (mounted) setState(() => _error = '$e');
    });
  }

  @override
  Widget build(BuildContext context) {
    final s = widget.app.server;
    final t = Theme.of(context);
    final lease = _status?['lease'];
    return ListenableBuilder(
      listenable: Privacy.instance,
      builder: (context, _) => ListView(padding: const EdgeInsets.symmetric(vertical: 8), children: [
        const Padding(padding: EdgeInsets.fromLTRB(16, 8, 16, 0), child: SectionTitle('Server')),
        ListTile(leading: const Icon(Icons.dns_outlined), title: Text(s?.address ?? 'Not connected'), subtitle: Text(s == null ? '' : 'Fingerprint ${shortFingerprint(s.fingerprint)} · this phone is “${s.deviceName}”')),
        if (_status != null)
          ListTile(
            leading: const Icon(Icons.info_outline),
            title: Text('FinGraph Server ${_status!['version']}'),
            subtitle: Text([
              if (lease != null) 'Being edited on ${lease['device']}',
              if (_status!['last_backup'] != null) 'Last backup ${_status!['last_backup']}',
              if ((_status!['inbox'] as num? ?? 0) > 0) '${_status!['inbox']} in the inbox',
              if (((_status!['needs_attention'] as List?) ?? []).isNotEmpty) '${(_status!['needs_attention'] as List).length} needing attention',
            ].join(' · ')),
          ),
        if (_error != null) ListTile(leading: Icon(Icons.cloud_off, color: t.colorScheme.error), title: Text(_error!)),
        ListTile(
          leading: Icon(Icons.link_off, color: t.colorScheme.error),
          title: const Text('Disconnect this phone'),
          subtitle: const Text('Unpairs it from the server and forgets what it kept'),
          onTap: () async {
            final ok = await showDialog<bool>(
              context: context,
              builder: (c) => AlertDialog(
                title: const Text('Disconnect this phone?'),
                content: const Text('It stops reaching the server, and the budget and transactions kept on it are deleted. You can pair it again with a new code.'),
                actions: [TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('Cancel')), FilledButton(onPressed: () => Navigator.pop(c, true), child: const Text('Disconnect'))],
              ),
            );
            if (ok != true) return;
            await s?.disconnect();
            await clearCache();
            if (context.mounted) Navigator.popUntil(context, (r) => r.isFirst);
            widget.app.setServer(null);
          },
        ),
        const Padding(padding: EdgeInsets.fromLTRB(16, 8, 16, 0), child: SectionTitle('Privacy')),
        SwitchListTile(
          secondary: const Icon(Icons.fingerprint),
          title: const Text('App lock'),
          subtitle: const Text('Fingerprint, face or the phone’s PIN to open FinGraph'),
          value: Privacy.instance.lock,
          onChanged: (v) => Privacy.instance.setLock(v, context),
        ),
        SwitchListTile(
          secondary: const Icon(Icons.screenshot_monitor_outlined),
          title: const Text('Hide from screenshots'),
          subtitle: const Text('Blocks screenshots and blanks FinGraph in the recent apps list'),
          value: Privacy.instance.secure,
          onChanged: Privacy.instance.setSecure,
        ),
      ]),
    );
  }
}
