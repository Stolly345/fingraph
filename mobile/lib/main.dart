import 'package:flutter/material.dart';
import 'package:fingraph/src/rust/api/hello.dart';
import 'package:fingraph/src/rust/frb_generated.dart';

Future<void> main() async {
  await RustLib.init();
  runApp(const FinGraphApp());
}

class FinGraphApp extends StatelessWidget {
  const FinGraphApp({super.key});

  @override
  Widget build(BuildContext context) {
    const seed = Color(0xFF1C6E8C);
    return MaterialApp(
      title: 'FinGraph',
      theme: ThemeData(colorSchemeSeed: seed, useMaterial3: true),
      darkTheme: ThemeData(colorSchemeSeed: seed, brightness: Brightness.dark, useMaterial3: true),
      home: const CoreCheckPage(),
    );
  }
}

/// Phase 0: shows that the FinGraph core runs on this phone.
class CoreCheckPage extends StatelessWidget {
  const CoreCheckPage({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('FinGraph')),
      body: FutureBuilder<CoreCheck>(
        future: coreCheck(),
        builder: (context, snap) {
          if (snap.hasError) {
            return _Message(icon: Icons.error_outline, text: 'The core didn\'t start: ${snap.error}');
          }
          final c = snap.data;
          if (c == null) {
            return const Center(child: CircularProgressIndicator());
          }
          return ListView(
            padding: const EdgeInsets.all(16),
            children: [
              ListTile(leading: const Icon(Icons.check_circle_outline), title: const Text('Core version'), subtitle: Text(c.version)),
              ListTile(leading: const Icon(Icons.store_outlined), title: const Text('Businesses in the directory'), subtitle: Text('${c.directory}')),
              ListTile(
                leading: const Icon(Icons.receipt_long_outlined),
                title: const Text('Demo statements read and imported'),
                subtitle: Text('${c.demoTransactions} transactions'),
              ),
              ListTile(leading: const Icon(Icons.search), title: const Text('Sample recognition'), subtitle: Text(c.sample)),
            ],
          );
        },
      ),
    );
  }
}

class _Message extends StatelessWidget {
  const _Message({required this.icon, required this.text});
  final IconData icon;
  final String text;

  @override
  Widget build(BuildContext context) => Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: Column(mainAxisSize: MainAxisSize.min, children: [Icon(icon, size: 48), const SizedBox(height: 12), Text(text, textAlign: TextAlign.center)]),
        ),
      );
}
