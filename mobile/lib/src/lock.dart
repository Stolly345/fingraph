// The app lock (fingerprint, face or the phone's PIN) and hiding FinGraph from screenshots and the
// recent apps list.
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:local_auth/local_auth.dart';

class Privacy extends ChangeNotifier {
  Privacy._();
  static final instance = Privacy._();
  static const _storage = FlutterSecureStorage();
  static const _window = MethodChannel('fingraph/window');

  bool lock = false;
  bool secure = true;

  Future<void> load() async {
    lock = await _storage.read(key: 'lock') == '1';
    secure = await _storage.read(key: 'secure') != '0';
    await _apply();
  }

  Future<void> _apply() async {
    try {
      await _window.invokeMethod<void>('secure', secure);
    } catch (_) {}
  }

  Future<void> setSecure(bool v) async {
    secure = v;
    await _storage.write(key: 'secure', value: v ? '1' : '0');
    await _apply();
    notifyListeners();
  }

  /// Turning the lock on asks for the fingerprint (or PIN) once, so nobody locks themselves out.
  Future<void> setLock(bool v, BuildContext context) async {
    if (v) {
      final auth = LocalAuthentication();
      if (!await auth.isDeviceSupported()) {
        if (context.mounted) {
          ScaffoldMessenger.of(context).showSnackBar(const SnackBar(content: Text('Set a screen lock (PIN, pattern or fingerprint) on the phone first.')));
        }
        return;
      }
      if (!await unlock()) return;
    }
    lock = v;
    await _storage.write(key: 'lock', value: v ? '1' : '0');
    notifyListeners();
  }

  static Future<bool> unlock() async {
    try {
      return await LocalAuthentication().authenticate(localizedReason: 'Unlock FinGraph', persistAcrossBackgrounding: true);
    } catch (_) {
      return false;
    }
  }
}

/// Shows `child` only after unlocking, when the lock is on; locks again after a minute away.
class LockGate extends StatefulWidget {
  const LockGate({super.key, required this.child});
  final Widget child;

  @override
  State<LockGate> createState() => _LockGateState();
}

class _LockGateState extends State<LockGate> with WidgetsBindingObserver {
  bool _locked = Privacy.instance.lock;
  DateTime? _away;
  bool _asking = false;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    if (_locked) WidgetsBinding.instance.addPostFrameCallback((_) => _ask());
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (!Privacy.instance.lock || _asking) return;
    if (state == AppLifecycleState.paused || state == AppLifecycleState.hidden) {
      _away ??= DateTime.now();
    } else if (state == AppLifecycleState.resumed) {
      final away = _away;
      _away = null;
      if (away != null && DateTime.now().difference(away) > const Duration(minutes: 1)) {
        setState(() => _locked = true);
        _ask();
      }
    }
  }

  Future<void> _ask() async {
    if (_asking) return;
    _asking = true;
    final ok = await Privacy.unlock();
    _asking = false;
    if (mounted && ok) setState(() => _locked = false);
  }

  @override
  Widget build(BuildContext context) {
    if (!_locked) return widget.child;
    final t = Theme.of(context);
    return Scaffold(
      body: Center(
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          Icon(Icons.lock_outline, size: 56, color: t.colorScheme.primary),
          const SizedBox(height: 16),
          Text('FinGraph is locked', style: t.textTheme.titleLarge),
          const SizedBox(height: 24),
          FilledButton.icon(onPressed: _ask, icon: const Icon(Icons.fingerprint), label: const Text('Unlock')),
        ]),
      ),
    );
  }
}
