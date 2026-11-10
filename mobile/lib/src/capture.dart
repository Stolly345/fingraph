// What the phone adds: a statement PDF (shared from the bank's app or picked), a quick purchase, a
// receipt photo. Each goes to the server, which applies it to the ledger (or holds it while someone is
// editing on a computer).
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:image_picker/image_picker.dart';
import 'package:intl/intl.dart';

import 'data.dart';

const _share = MethodChannel('fingraph/share');

class SharedFile {
  SharedFile(this.name, this.bytes);
  final String name;
  final Uint8List bytes;
}

SharedFile? _fromMap(dynamic m) {
  if (m is! Map) return null;
  final bytes = m['bytes'];
  if (bytes is! Uint8List) return null;
  return SharedFile(m['name'] as String? ?? 'statement.pdf', bytes);
}

/// A PDF shared to FinGraph (from the bank's app, Files, an email), if the app was opened that way.
Future<SharedFile?> takeShared() async {
  try {
    return _fromMap(await _share.invokeMethod<dynamic>('take'));
  } catch (_) {
    return null;
  }
}

/// Calls `on` when a PDF is shared to FinGraph while it's open.
void listenForShares(void Function(SharedFile) on) {
  _share.setMethodCallHandler((call) async {
    if (call.method == 'shared') {
      final f = _fromMap(call.arguments);
      if (f != null) on(f);
    }
  });
}

/// Picks a PDF with the phone's file picker.
Future<SharedFile?> pickPdf() async {
  try {
    return _fromMap(await _share.invokeMethod<dynamic>('pick'));
  } catch (_) {
    return null;
  }
}

/// Shows a message; taken before any waiting, so it works after the page that asked has gone.
void Function(String) _sayer(BuildContext context) {
  final m = ScaffoldMessenger.of(context);
  return (text) => m.showSnackBar(SnackBar(content: Text(text)));
}

/// Sends a statement to the server's inbox, after asking.
Future<void> sendStatement(BuildContext context, AppState app, SharedFile f) async {
  final say = _sayer(context);
  final s = app.server;
  if (s == null) {
    say('Connect to a server first.');
    return;
  }
  final ok = await showDialog<bool>(
    context: context,
    builder: (c) => AlertDialog(
      icon: const Icon(Icons.upload_file),
      title: const Text('Send this statement to FinGraph?'),
      content: Text('${f.name}\n\nThe server reads it, imports it with the same checks as the desktop and files it under its month.'),
      actions: [TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('Cancel')), FilledButton(onPressed: () => Navigator.pop(c, true), child: const Text('Send'))],
    ),
  );
  if (ok != true) return;
  try {
    final name = await s.sendStatement(f.name, f.bytes);
    say('Sent $name. It shows up once the server has read it.');
    Future.delayed(const Duration(seconds: 20), app.refresh);
  } catch (e) {
    say("Couldn't send it: $e");
  }
}

/// Takes a receipt photo and attaches it to a transaction.
Future<void> addReceipt(BuildContext context, AppState app, int transactionId) async {
  final say = _sayer(context);
  final s = app.server;
  if (s == null) return;
  final source = await showModalBottomSheet<ImageSource>(
    context: context,
    showDragHandle: true,
    builder: (c) => SafeArea(
      child: Column(mainAxisSize: MainAxisSize.min, children: [
        ListTile(leading: const Icon(Icons.photo_camera_outlined), title: const Text('Take a photo'), onTap: () => Navigator.pop(c, ImageSource.camera)),
        ListTile(leading: const Icon(Icons.photo_library_outlined), title: const Text('Choose from the gallery'), onTap: () => Navigator.pop(c, ImageSource.gallery)),
      ]),
    ),
  );
  if (source == null) return;
  final photo = await ImagePicker().pickImage(source: source, maxWidth: 2200, imageQuality: 75);
  if (photo == null) return;
  try {
    final r = await s.addReceipt(transactionId, await photo.readAsBytes());
    say(r['queued'] == true ? 'Receipt saved. It’s added when the computer editing the ledger is done.' : 'Receipt added.');
    app.refresh();
  } catch (e) {
    say("Couldn't add the receipt: $e");
  }
}

/// A purchase the statement hasn't shown yet (cash, or this week's card spending).
Future<void> quickAdd(BuildContext context, AppState app) async {
  final say = _sayer(context);
  final s = app.server;
  if (s == null) return;
  List<dynamic> accounts;
  try {
    accounts = ((await app.view('accounts')).data['accounts'] as List?) ?? [];
  } catch (e) {
    say("Couldn't load the accounts: $e");
    return;
  }
  if (accounts.isEmpty) {
    say('The ledger has no accounts yet.');
    return;
  }
  if (!context.mounted) return;
  final result = await showModalBottomSheet<Map<String, dynamic>>(
    context: context,
    isScrollControlled: true,
    showDragHandle: true,
    builder: (c) => _QuickAdd(accounts: accounts),
  );
  if (result == null) return;
  try {
    final r = await s.addPurchase(result);
    say(r['queued'] == true ? 'Saved. It’s added when the computer editing the ledger is done.' : 'Added ${result['payee']}.');
    app.refresh();
  } catch (e) {
    say("Couldn't add it: $e");
  }
}

class _QuickAdd extends StatefulWidget {
  const _QuickAdd({required this.accounts});
  final List<dynamic> accounts;
  @override
  State<_QuickAdd> createState() => _QuickAddState();
}

class _QuickAddState extends State<_QuickAdd> {
  final _amount = TextEditingController();
  final _payee = TextEditingController();
  late int _account = (widget.accounts.first['id'] as num).toInt();
  DateTime _date = DateTime.now();
  bool _income = false;

  @override
  Widget build(BuildContext context) {
    final t = Theme.of(context);
    final amount = double.tryParse(_amount.text.replaceAll(',', ''));
    final ready = amount != null && amount > 0 && _payee.text.trim().isNotEmpty;
    return Padding(
      padding: EdgeInsets.fromLTRB(24, 0, 24, 24 + MediaQuery.of(context).viewInsets.bottom),
      child: Column(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        Text('Add a purchase', style: t.textTheme.titleLarge),
        const SizedBox(height: 4),
        Text('When the statement with it arrives, FinGraph keeps the statement’s line instead.', style: t.textTheme.bodySmall),
        const SizedBox(height: 16),
        SegmentedButton<bool>(
          segments: const [ButtonSegment(value: false, label: Text('Money out')), ButtonSegment(value: true, label: Text('Money in'))],
          selected: {_income},
          onSelectionChanged: (v) => setState(() => _income = v.first),
        ),
        const SizedBox(height: 12),
        TextField(
          controller: _amount,
          autofocus: true,
          keyboardType: const TextInputType.numberWithOptions(decimal: true),
          inputFormatters: [FilteringTextInputFormatter.allow(RegExp(r'[0-9.,]'))],
          decoration: const InputDecoration(labelText: 'Amount', border: OutlineInputBorder()),
          onChanged: (_) => setState(() {}),
        ),
        const SizedBox(height: 12),
        TextField(controller: _payee, textCapitalization: TextCapitalization.words, decoration: const InputDecoration(labelText: 'Where', border: OutlineInputBorder()), onChanged: (_) => setState(() {})),
        const SizedBox(height: 12),
        DropdownButtonFormField<int>(
          initialValue: _account,
          decoration: const InputDecoration(labelText: 'Account', border: OutlineInputBorder()),
          items: [for (final a in widget.accounts) DropdownMenuItem(value: (a['id'] as num).toInt(), child: Text('${a['name']}'))],
          onChanged: (v) => setState(() => _account = v ?? _account),
        ),
        const SizedBox(height: 12),
        OutlinedButton.icon(
          icon: const Icon(Icons.event),
          label: Text(DateFormat('EEE d MMM yyyy').format(_date)),
          onPressed: () async {
            final d = await showDatePicker(context: context, initialDate: _date, firstDate: DateTime.now().subtract(const Duration(days: 120)), lastDate: DateTime.now());
            if (d != null) setState(() => _date = d);
          },
        ),
        const SizedBox(height: 20),
        FilledButton(
          onPressed: !ready
              ? null
              : () => Navigator.pop(context, {
                    'account': _account,
                    'date': DateFormat('yyyy-MM-dd').format(_date),
                    'payee': _payee.text.trim(),
                    'amount': (_income ? amount : -amount).toStringAsFixed(2),
                  }),
          child: const Text('Add'),
        ),
      ]),
    );
  }
}
