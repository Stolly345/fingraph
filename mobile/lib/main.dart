import 'package:flutter/material.dart';

import 'src/capture.dart';
import 'src/connection.dart';
import 'src/data.dart';
import 'src/lock.dart';
import 'src/pages/budget.dart';
import 'src/pages/connect.dart';
import 'src/pages/more.dart';
import 'src/pages/summary.dart';
import 'src/pages/transactions.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await Privacy.instance.load();
  final server = await Server.load();
  runApp(FinGraphApp(app: AppState(server)));
}

class FinGraphApp extends StatelessWidget {
  const FinGraphApp({super.key, required this.app});
  final AppState app;

  @override
  Widget build(BuildContext context) {
    const seed = Color(0xFF1C6E8C);
    return MaterialApp(
      title: 'FinGraph',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(colorSchemeSeed: seed, useMaterial3: true),
      darkTheme: ThemeData(colorSchemeSeed: seed, brightness: Brightness.dark, useMaterial3: true),
      home: LockGate(
        child: ListenableBuilder(
          listenable: app,
          builder: (context, _) => app.server == null ? ConnectPage(app: app) : HomeShell(app: app),
        ),
      ),
    );
  }
}

class HomeShell extends StatefulWidget {
  const HomeShell({super.key, required this.app});
  final AppState app;

  @override
  State<HomeShell> createState() => _HomeShellState();
}

class _HomeShellState extends State<HomeShell> {
  int _tab = 0;

  @override
  void initState() {
    super.initState();
    // a statement shared to FinGraph, now or later
    listenForShares((f) => sendStatement(context, widget.app, f));
    WidgetsBinding.instance.addPostFrameCallback((_) async {
      final f = await takeShared();
      if (f != null && mounted) await sendStatement(context, widget.app, f);
    });
  }

  @override
  Widget build(BuildContext context) {
    final app = widget.app;
    final pages = [SummaryPage(app: app), BudgetPage(app: app), TransactionsPage(app: app), MorePage(app: app)];
    const titles = ['FinGraph', 'Budget', 'Transactions', 'More'];
    return Scaffold(
      appBar: AppBar(
        title: Text(titles[_tab]),
        actions: [
          PopupMenuButton<String>(
            icon: const Icon(Icons.add),
            tooltip: 'Add',
            onSelected: (v) async {
              if (v == 'purchase') await quickAdd(context, app);
              if (v == 'statement') {
                final f = await pickPdf();
                if (f != null && context.mounted) await sendStatement(context, app, f);
              }
            },
            itemBuilder: (_) => const [
              PopupMenuItem(value: 'purchase', child: ListTile(leading: Icon(Icons.shopping_bag_outlined), title: Text('A purchase'))),
              PopupMenuItem(value: 'statement', child: ListTile(leading: Icon(Icons.upload_file), title: Text('A statement PDF'))),
            ],
          ),
        ],
      ),
      body: SafeArea(top: false, child: IndexedStack(index: _tab, children: pages)),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (i) => setState(() => _tab = i),
        destinations: const [
          NavigationDestination(icon: Icon(Icons.space_dashboard_outlined), selectedIcon: Icon(Icons.space_dashboard), label: 'Summary'),
          NavigationDestination(icon: Icon(Icons.pie_chart_outline), selectedIcon: Icon(Icons.pie_chart), label: 'Budget'),
          NavigationDestination(icon: Icon(Icons.receipt_long_outlined), selectedIcon: Icon(Icons.receipt_long), label: 'Transactions'),
          NavigationDestination(icon: Icon(Icons.more_horiz), label: 'More'),
        ],
      ),
    );
  }
}
