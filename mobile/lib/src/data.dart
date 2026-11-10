// The app's shared state (the server, the month and workspace on screen) and the views from the server,
// kept on the phone so the last answers still show without a connection.
import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:intl/intl.dart';
import 'package:path_provider/path_provider.dart';

import 'connection.dart';

class Loaded {
  Loaded(this.data, this.at, {this.offline = false, this.error});
  final Map<String, dynamic> data;
  final DateTime at;
  final bool offline;
  final String? error;
}

class AppState extends ChangeNotifier {
  AppState(this.server);
  Server? server;

  /// The month on screen (first day).
  DateTime month = DateTime(DateTime.now().year, DateTime.now().month);

  /// One workspace, or null for all.
  int? workspace;

  /// Bumped to make every page fetch again (pull to refresh, after sending something).
  int generation = 0;

  void setServer(Server? s) {
    server = s;
    generation++;
    notifyListeners();
  }

  void shiftMonth(int by) {
    month = DateTime(month.year, month.month + by);
    notifyListeners();
  }

  void setWorkspace(int? id) {
    workspace = id;
    notifyListeners();
  }

  void refresh() {
    generation++;
    notifyListeners();
  }

  bool get isCurrentMonth => month.year == DateTime.now().year && month.month == DateTime.now().month;

  Map<String, String> query([Map<String, String> extra = const {}]) => {
        'month': DateFormat('yyyy-MM').format(month),
        if (workspace != null) 'workspace': '$workspace',
        ...extra,
      };

  /// A view from the server, or the last copy kept on the phone when the server can't be reached.
  Future<Loaded> view(String name, [Map<String, String> extra = const {}]) async {
    final s = server;
    final q = query(extra);
    final key = '$name?${Uri(queryParameters: q).query}';
    try {
      if (s == null) throw ServerError('Not connected to a server.');
      final data = await s.view(name, q);
      await _Cache.put(key, data);
      return Loaded(data, DateTime.now());
    } on ServerError catch (e) {
      final cached = await _Cache.get(key);
      if (cached != null) return Loaded(cached.$1, cached.$2, offline: true, error: e.message);
      rethrow;
    }
  }
}

class _Cache {
  static Future<File> _file(String key) async {
    final dir = Directory('${(await getApplicationSupportDirectory()).path}/views');
    await dir.create(recursive: true);
    final name = base64Url.encode(utf8.encode(key)).replaceAll('=', '');
    return File('${dir.path}/$name.json');
  }

  static Future<void> put(String key, Map<String, dynamic> data) async {
    try {
      await (await _file(key)).writeAsString(jsonEncode({'at': DateTime.now().toIso8601String(), 'data': data}));
    } catch (_) {}
  }

  static Future<(Map<String, dynamic>, DateTime)?> get(String key) async {
    try {
      final f = await _file(key);
      if (!await f.exists()) return null;
      final j = jsonDecode(await f.readAsString()) as Map<String, dynamic>;
      return (j['data'] as Map<String, dynamic>, DateTime.parse(j['at'] as String));
    } catch (_) {
      return null;
    }
  }

  static Future<void> clear() async {
    try {
      final dir = Directory('${(await getApplicationSupportDirectory()).path}/views');
      if (await dir.exists()) await dir.delete(recursive: true);
    } catch (_) {}
  }
}

/// Forgets every kept view (after disconnecting).
Future<void> clearCache() => _Cache.clear();

// ------------------------------------------------------------------ formatting

String money(dynamic amount, String symbol, {bool sign = false}) {
  final v = amount is num ? amount.toDouble() : double.tryParse('$amount') ?? 0;
  final f = NumberFormat.currency(symbol: symbol, decimalDigits: 2).format(v.abs());
  if (v < 0) return '−$f';
  return sign && v > 0 ? '+$f' : f;
}

double number(dynamic v) => v is num ? v.toDouble() : double.tryParse('$v') ?? 0;

String day(String iso) {
  final d = DateTime.tryParse(iso);
  return d == null ? iso : DateFormat('EEE d MMM').format(d);
}

String shortDay(String iso) {
  final d = DateTime.tryParse(iso);
  return d == null ? iso : DateFormat('d MMM').format(d);
}

String asOf(DateTime t) {
  final now = DateTime.now();
  final sameDay = t.year == now.year && t.month == now.month && t.day == now.day;
  return sameDay ? DateFormat('HH:mm').format(t) : DateFormat('d MMM HH:mm').format(t);
}
