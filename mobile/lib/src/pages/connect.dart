import 'package:flutter/material.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

import '../connection.dart';
import '../data.dart';

/// Pairing with a FinGraph server: scan the QR code it prints, or type its address and the code.
class ConnectPage extends StatefulWidget {
  const ConnectPage({super.key, required this.app});
  final AppState app;

  @override
  State<ConnectPage> createState() => _ConnectPageState();
}

class _ConnectPageState extends State<ConnectPage> {
  final _address = TextEditingController();
  final _code = TextEditingController();
  final _name = TextEditingController(text: 'Phone');
  bool _busy = false;
  String? _error;

  Future<void> _pair(String address, String code, String? fingerprint) async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      var fp = fingerprint;
      if (fp == null) {
        // typed by hand: show the fingerprint to compare with the one the server printed
        fp = await probe(address);
        if (!mounted) return;
        final same = await showDialog<bool>(
          context: context,
          builder: (c) => AlertDialog(
            title: const Text('Is this your server?'),
            content: Text('The server printed a fingerprint next to the pairing code. It should be:\n\n${shortFingerprint(fp!)}'),
            actions: [TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('No')), FilledButton(onPressed: () => Navigator.pop(c, true), child: const Text('Yes, it matches'))],
          ),
        );
        if (same != true) {
          setState(() => _busy = false);
          return;
        }
      }
      final s = await Server.pair(address, code, fp, _name.text.trim().isEmpty ? 'Phone' : _name.text.trim());
      await s.save();
      widget.app.setServer(s);
    } on ServerError catch (e) {
      if (mounted) setState(() => _error = e.message);
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _scan() async {
    final text = await Navigator.push<String>(context, MaterialPageRoute(builder: (_) => const _Scanner()));
    if (text == null) return;
    final link = PairingLink.parse(text);
    if (link == null) {
      setState(() => _error = "That QR code isn't a FinGraph pairing code.");
      return;
    }
    await _pair(link.address, link.code, link.fingerprint);
  }

  @override
  Widget build(BuildContext context) {
    final t = Theme.of(context);
    return Scaffold(
      body: SafeArea(
        child: ListView(padding: const EdgeInsets.all(24), children: [
          const SizedBox(height: 24),
          Icon(Icons.insights, size: 56, color: t.colorScheme.primary),
          const SizedBox(height: 16),
          Text('Connect to your FinGraph server', style: t.textTheme.headlineSmall, textAlign: TextAlign.center),
          const SizedBox(height: 8),
          Text('On the server, run\n\ndocker exec fingraph fingraph-server pair\n\nand scan the QR code it shows.',
              style: t.textTheme.bodyMedium?.copyWith(color: t.colorScheme.onSurfaceVariant), textAlign: TextAlign.center),
          const SizedBox(height: 24),
          TextField(controller: _name, decoration: const InputDecoration(labelText: 'This phone’s name', border: OutlineInputBorder(), helperText: 'How the server lists this phone')),
          const SizedBox(height: 16),
          FilledButton.icon(onPressed: _busy ? null : _scan, icon: const Icon(Icons.qr_code_scanner), label: const Text('Scan the QR code')),
          const SizedBox(height: 32),
          Row(children: [const Expanded(child: Divider()), Padding(padding: const EdgeInsets.symmetric(horizontal: 12), child: Text('or type it', style: t.textTheme.bodySmall)), const Expanded(child: Divider())]),
          const SizedBox(height: 16),
          TextField(controller: _address, keyboardType: TextInputType.url, autocorrect: false, decoration: const InputDecoration(labelText: 'Server address', hintText: '192.168.1.20:8743', border: OutlineInputBorder())),
          const SizedBox(height: 12),
          TextField(controller: _code, textCapitalization: TextCapitalization.characters, autocorrect: false, decoration: const InputDecoration(labelText: 'Pairing code', hintText: 'ABCD-EFGH', border: OutlineInputBorder())),
          const SizedBox(height: 16),
          OutlinedButton(
            onPressed: _busy ? null : () => _pair(_address.text, _code.text, null),
            child: _busy ? const SizedBox(width: 20, height: 20, child: CircularProgressIndicator(strokeWidth: 2)) : const Text('Connect'),
          ),
          if (_error != null) ...[
            const SizedBox(height: 16),
            Text(_error!, style: TextStyle(color: t.colorScheme.error), textAlign: TextAlign.center),
          ],
        ]),
      ),
    );
  }
}

class _Scanner extends StatefulWidget {
  const _Scanner();
  @override
  State<_Scanner> createState() => _ScannerState();
}

class _ScannerState extends State<_Scanner> {
  bool _done = false;
  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Scan the pairing QR code')),
      body: MobileScanner(
        onDetect: (capture) {
          final v = capture.barcodes.map((b) => b.rawValue).whereType<String>().firstOrNull;
          if (v == null || _done) return;
          _done = true;
          Navigator.pop(context, v);
        },
      ),
    );
  }
}
