import 'dart:async';

import 'package:flutter/material.dart';

import '../capture.dart';
import '../data.dart';
import '../widgets.dart';

/// The month's transactions, newest first; a search looks through all of them.
class TransactionsPage extends StatefulWidget {
  const TransactionsPage({super.key, required this.app});
  final AppState app;

  @override
  State<TransactionsPage> createState() => _TransactionsPageState();
}

class _TransactionsPageState extends State<TransactionsPage> {
  final _search = TextEditingController();
  Timer? _debounce;
  String _q = '';
  final List<Map<String, dynamic>> _items = [];
  Map<String, dynamic>? _head;
  Loaded? _loaded;
  int _page = 0;
  bool _more = false;
  bool _loading = false;
  String? _error;
  String _key = '';

  String get _wanted => '$_q|${widget.app.query()}|${widget.app.generation}';

  Future<void> _load({bool next = false}) async {
    if (_loading) return;
    final key = _wanted;
    setState(() {
      _loading = true;
      _error = null;
      if (!next) {
        _key = key;
        _page = 0;
      }
    });
    try {
      final l = await widget.app.view('transactions', {if (_q.isNotEmpty) 'q': _q, 'page': '${next ? _page + 1 : 0}'});
      if (!mounted || key != _wanted) return;
      setState(() {
        if (!next) _items.clear();
        _items.addAll(((l.data['items'] as List?) ?? []).cast<Map<String, dynamic>>());
        _page = (l.data['page'] as num?)?.toInt() ?? 0;
        _more = l.data['more'] == true;
        _head = l.data;
        _loaded = l;
      });
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  @override
  void dispose() {
    _debounce?.cancel();
    _search.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    if (_key != _wanted && !_loading) WidgetsBinding.instance.addPostFrameCallback((_) => _load());
    final sym = _head?['symbol'] as String? ?? r'$';
    final t = Theme.of(context);
    final rows = <Widget>[];
    String? lastDay;
    for (final tx in _items) {
      final d = tx['date'] as String;
      if (d != lastDay) {
        rows.add(Padding(padding: const EdgeInsets.fromLTRB(20, 16, 20, 4), child: Text(day(d), style: t.textTheme.labelLarge?.copyWith(color: t.colorScheme.onSurfaceVariant))));
        lastDay = d;
      }
      rows.add(_TxTile(tx: tx, symbol: sym, onTap: () => showTransaction(context, widget.app, tx, sym)));
    }
    return Column(children: [
      if (_q.isEmpty) MonthBar(app: widget.app, workspaces: (_head?['workspaces'] as List?) ?? []),
      Padding(
        padding: const EdgeInsets.fromLTRB(16, 4, 16, 8),
        child: SearchBar(
          controller: _search,
          hintText: 'Search all transactions',
          leading: const Icon(Icons.search),
          trailing: [if (_q.isNotEmpty) IconButton(icon: const Icon(Icons.close), onPressed: () => setState(() { _search.clear(); _q = ''; }))],
          onChanged: (v) {
            _debounce?.cancel();
            _debounce = Timer(const Duration(milliseconds: 400), () => setState(() => _q = v.trim()));
          },
        ),
      ),
      if (_loaded != null) Padding(padding: const EdgeInsets.symmetric(horizontal: 16), child: OfflineNote(_loaded!)),
      Expanded(
        child: _error != null && _items.isEmpty
            ? Message(icon: Icons.cloud_off, title: "Couldn't load transactions", text: _error, action: ('Try again', () => _load()))
            : _items.isEmpty && _loading
                ? const Center(child: CircularProgressIndicator())
                : _items.isEmpty
                    ? Message(icon: Icons.receipt_long_outlined, title: _q.isEmpty ? 'No transactions this month' : 'Nothing matches “$_q”')
                    : RefreshIndicator(
                        onRefresh: () => _load(),
                        child: NotificationListener<ScrollNotification>(
                          onNotification: (n) {
                            if (_more && !_loading && n.metrics.extentAfter < 400) _load(next: true);
                            return false;
                          },
                          child: ListView(padding: const EdgeInsets.only(bottom: 96), children: [
                            if (_head?['total'] != null)
                              Padding(padding: const EdgeInsets.fromLTRB(20, 0, 20, 0), child: Text('${_head!['total']} transactions', style: t.textTheme.bodySmall)),
                            ...rows,
                            if (_loading) const Padding(padding: EdgeInsets.all(16), child: Center(child: CircularProgressIndicator())),
                          ]),
                        ),
                      ),
      ),
    ]);
  }
}

class _TxTile extends StatelessWidget {
  const _TxTile({required this.tx, required this.symbol, required this.onTap});
  final Map<String, dynamic> tx;
  final String symbol;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final t = Theme.of(context);
    final amount = number(tx['amount']);
    final cur = tx['currency'] as String? ?? '';
    final cat = tx['category'] as String?;
    return ListTile(
      onTap: onTap,
      title: Text(tx['payee'] as String, maxLines: 1, overflow: TextOverflow.ellipsis),
      subtitle: Text([cat ?? (tx['transfer'] == true ? 'Transfer' : 'Not categorized yet'), tx['account']].join(' · '), maxLines: 1, overflow: TextOverflow.ellipsis),
      trailing: Column(mainAxisAlignment: MainAxisAlignment.center, crossAxisAlignment: CrossAxisAlignment.end, children: [
        Text('${money(amount, '', sign: true)} $cur', style: t.textTheme.titleSmall?.copyWith(color: amount > 0 ? Colors.green.shade700 : null)),
        if ((tx['attachments'] as num? ?? 0) > 0) Icon(Icons.attach_file, size: 14, color: t.colorScheme.onSurfaceVariant),
      ]),
    );
  }
}

/// A transaction's details, with adding a receipt photo.
void showTransaction(BuildContext context, AppState app, Map<String, dynamic> tx, String symbol) {
  showModalBottomSheet<void>(
    context: context,
    showDragHandle: true,
    isScrollControlled: true,
    builder: (context) {
      final t = Theme.of(context);
      final amount = number(tx['amount']);
      Widget row(String label, String? value) => value == null || value.isEmpty
          ? const SizedBox.shrink()
          : Padding(
              padding: const EdgeInsets.symmetric(vertical: 6),
              child: Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
                SizedBox(width: 110, child: Text(label, style: t.textTheme.bodyMedium?.copyWith(color: t.colorScheme.onSurfaceVariant))),
                Expanded(child: Text(value, style: t.textTheme.bodyMedium)),
              ]),
            );
      return SafeArea(
        child: Padding(
          padding: const EdgeInsets.fromLTRB(24, 0, 24, 24),
          child: Column(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.start, children: [
            Text(tx['payee'] as String, style: t.textTheme.titleLarge),
            const SizedBox(height: 4),
            Text('${money(amount, '', sign: true)} ${tx['currency']}', style: t.textTheme.headlineSmall?.copyWith(color: amount > 0 ? Colors.green.shade700 : null)),
            const SizedBox(height: 16),
            row('Date', day(tx['date'] as String)),
            row('Category', [tx['parent_category'], tx['category']].whereType<String>().join(' › ')),
            row('Account', tx['account'] as String?),
            row('On the statement', tx['description'] as String?),
            row('Memo', tx['memo'] as String?),
            row('Tag', tx['tag'] as String?),
            row('Receipts', (tx['attachments'] as num? ?? 0) > 0 ? '${tx['attachments']}' : null),
            const SizedBox(height: 16),
            FilledButton.tonalIcon(
              onPressed: () async {
                Navigator.pop(context);
                await addReceipt(context, app, (tx['id'] as num).toInt());
              },
              icon: const Icon(Icons.photo_camera_outlined),
              label: const Text('Add a receipt photo'),
            ),
          ]),
        ),
      );
    },
  );
}
