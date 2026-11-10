// Pieces every page uses: the month switcher, loading and error states, the offline note.
import 'package:flutter/material.dart';
import 'package:intl/intl.dart';

import 'data.dart';

/// `‹ November 2026 ›` with the workspace picker beside it when there are several.
class MonthBar extends StatelessWidget implements PreferredSizeWidget {
  const MonthBar({super.key, required this.app, this.workspaces = const []});
  final AppState app;
  final List<dynamic> workspaces;

  @override
  Size get preferredSize => const Size.fromHeight(48);

  @override
  Widget build(BuildContext context) {
    final label = DateFormat('MMMM yyyy').format(app.month);
    return SizedBox(
      height: 48,
      child: Row(children: [
        IconButton(tooltip: 'Previous month', icon: const Icon(Icons.chevron_left), onPressed: () => app.shiftMonth(-1)),
        Expanded(child: Center(child: Text(label, style: Theme.of(context).textTheme.titleMedium))),
        IconButton(tooltip: 'Next month', icon: const Icon(Icons.chevron_right), onPressed: app.isCurrentMonth ? null : () => app.shiftMonth(1)),
        if (workspaces.length > 1)
          PopupMenuButton<int?>(
            tooltip: 'Workspace',
            icon: const Icon(Icons.workspaces_outline),
            initialValue: app.workspace,
            onSelected: app.setWorkspace,
            itemBuilder: (_) => [
              const PopupMenuItem<int?>(value: null, child: Text('All workspaces')),
              for (final w in workspaces) PopupMenuItem<int?>(value: (w['id'] as num).toInt(), child: Text(w['name'] as String)),
            ],
          ),
      ]),
    );
  }
}

/// Loads a view and shows it, with pull to refresh, a spinner the first time and the error if it fails.
class ViewLoader extends StatefulWidget {
  const ViewLoader({super.key, required this.app, required this.name, required this.builder, this.extra = const {}});
  final AppState app;
  final String name;
  final Map<String, String> extra;
  final Widget Function(BuildContext, Loaded) builder;

  @override
  State<ViewLoader> createState() => _ViewLoaderState();
}

class _ViewLoaderState extends State<ViewLoader> {
  Future<Loaded>? _future;
  String _key = '';
  Loaded? _last;

  String get _wanted => '${widget.name}|${widget.app.query(widget.extra)}|${widget.app.generation}';

  void _load() {
    _key = _wanted;
    _future = widget.app.view(widget.name, widget.extra).then((l) => _last = l);
  }

  @override
  Widget build(BuildContext context) {
    if (_future == null || _key != _wanted) _load();
    return FutureBuilder<Loaded>(
      future: _future,
      builder: (context, snap) {
        final data = snap.data ?? (snap.connectionState == ConnectionState.waiting ? _last : null);
        if (snap.hasError && data == null) {
          return Message(icon: Icons.cloud_off, title: "Couldn't load this", text: '${snap.error}', action: ('Try again', () => setState(_load)));
        }
        if (data == null) return const Center(child: CircularProgressIndicator());
        return RefreshIndicator(
          onRefresh: () async {
            setState(_load);
            await _future?.catchError((_) => _last!);
          },
          child: widget.builder(context, data),
        );
      },
    );
  }
}

class Message extends StatelessWidget {
  const Message({super.key, required this.icon, required this.title, this.text, this.action});
  final IconData icon;
  final String title;
  final String? text;
  final (String, VoidCallback)? action;

  @override
  Widget build(BuildContext context) {
    final t = Theme.of(context);
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(32),
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          Icon(icon, size: 48, color: t.colorScheme.outline),
          const SizedBox(height: 16),
          Text(title, style: t.textTheme.titleMedium, textAlign: TextAlign.center),
          if (text != null) ...[const SizedBox(height: 8), Text(text!, style: t.textTheme.bodyMedium?.copyWith(color: t.colorScheme.onSurfaceVariant), textAlign: TextAlign.center)],
          if (action != null) ...[const SizedBox(height: 16), FilledButton.tonal(onPressed: action!.$2, child: Text(action!.$1))],
        ]),
      ),
    );
  }
}

/// "Offline · as of 08:14" above a page showing kept data.
class OfflineNote extends StatelessWidget {
  const OfflineNote(this.loaded, {super.key});
  final Loaded loaded;

  @override
  Widget build(BuildContext context) {
    if (!loaded.offline) return const SizedBox.shrink();
    final t = Theme.of(context);
    return Material(
      color: t.colorScheme.secondaryContainer,
      borderRadius: BorderRadius.circular(12),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
        child: Row(children: [
          Icon(Icons.cloud_off, size: 18, color: t.colorScheme.onSecondaryContainer),
          const SizedBox(width: 12),
          Expanded(child: Text("Can't reach the server · showing what it sent at ${asOf(loaded.at)}", style: TextStyle(color: t.colorScheme.onSecondaryContainer))),
        ]),
      ),
    );
  }
}

class SectionTitle extends StatelessWidget {
  const SectionTitle(this.text, {super.key});
  final String text;
  @override
  Widget build(BuildContext context) => Padding(
        padding: const EdgeInsets.fromLTRB(4, 24, 4, 8),
        child: Text(text, style: Theme.of(context).textTheme.titleSmall?.copyWith(color: Theme.of(context).colorScheme.primary)),
      );
}

/// Spent against a budget, red when over.
class Meter extends StatelessWidget {
  const Meter({super.key, required this.spent, required this.budget, this.state = 'ok'});
  final double spent;
  final double budget;
  final String state;

  @override
  Widget build(BuildContext context) {
    final c = Theme.of(context).colorScheme;
    final v = budget <= 0 ? (spent > 0 ? 1.0 : 0.0) : (spent / budget).clamp(0.0, 1.0);
    final color = switch (state) { 'over' => c.error, 'near' => Colors.orange.shade700, _ => c.primary };
    return ClipRRect(
      borderRadius: BorderRadius.circular(4),
      child: LinearProgressIndicator(value: v.toDouble(), minHeight: 8, color: color, backgroundColor: c.surfaceContainerHighest),
    );
  }
}
