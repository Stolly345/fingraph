// Talks to the FinGraph server. The server makes its own certificate, so instead of a certificate
// authority the app trusts exactly one certificate: the one whose SHA-256 fingerprint it learned when
// pairing (from the QR code, or bound into the pairing code's proof).
import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';

const defaultPort = 8743;

class ServerError implements Exception {
  ServerError(this.message, {this.status = 0});
  final String message;
  final int status;
  bool get notPaired => status == 401;
  bool get unreachable => status == 0;
  @override
  String toString() => message;
}

String fingerprintOf(X509Certificate c) => sha256.convert(c.der).toString();

/// `AB12 CD34 EF56 7890`, as the server prints it.
String shortFingerprint(String fp) {
  final s = fp.toUpperCase().padRight(16).substring(0, 16);
  return [for (var i = 0; i < 16; i += 4) s.substring(i, i + 4)].join(' ');
}

String normalizeCode(String code) => code.toUpperCase().replaceAll(RegExp(r'[^A-Z0-9]'), '');

/// What the server checks: the code bound to the certificate this phone sees.
String pairingProof(String code, String fingerprint) =>
    sha256.convert(utf8.encode('fingraph-pair:${normalizeCode(code)}:${fingerprint.toLowerCase()}')).toString();

/// `server`, `server:8743`, `https://server:8743/` → `server:8743`
String normalizeAddress(String a) {
  var s = a.trim().replaceFirst(RegExp(r'^https?://'), '');
  s = s.split('/').first;
  final hasPort = s.startsWith('[') ? s.contains(']:') : ':'.allMatches(s).length == 1;
  return hasPort || s.isEmpty ? s : '$s:$defaultPort';
}

/// What a pairing QR code holds: `fingraph://pair?a=ADDRESS&c=CODE&f=FINGERPRINT`.
class PairingLink {
  PairingLink(this.address, this.code, this.fingerprint);
  final String address;
  final String code;
  final String? fingerprint;

  static PairingLink? parse(String text) {
    final u = Uri.tryParse(text.trim());
    if (u == null || u.scheme != 'fingraph' || u.host != 'pair') return null;
    final a = u.queryParameters['a'];
    final c = u.queryParameters['c'];
    if (a == null || c == null) return null;
    return PairingLink(a, c, u.queryParameters['f']);
  }
}

class _Raw {
  _Raw(this.status, this.body, this.fingerprint);
  final int status;
  final List<int> body;
  final String? fingerprint;
  dynamic get json {
    try {
      return jsonDecode(utf8.decode(body));
    } catch (_) {
      return null;
    }
  }

  String get error => (json is Map && json['error'] is String) ? json['error'] as String : 'The server answered $status.';
}

Future<_Raw> _request(String address, String? pin, String? token, String method, String path,
    {Object? json, List<int>? bytes, String contentType = 'application/octet-stream', Duration timeout = const Duration(seconds: 20)}) async {
  String? seen;
  final client = HttpClient()
    ..connectionTimeout = const Duration(seconds: 8)
    ..badCertificateCallback = (cert, host, port) {
      seen = fingerprintOf(cert);
      return pin == null || seen == pin.toLowerCase();
    };
  try {
    final uri = Uri.parse('https://${normalizeAddress(address)}$path');
    final req = await client.openUrl(method, uri).timeout(timeout);
    req.headers.set(HttpHeaders.userAgentHeader, 'fingraph-android');
    if (token != null) req.headers.set(HttpHeaders.authorizationHeader, 'Bearer $token');
    if (json != null) {
      final b = utf8.encode(jsonEncode(json));
      req.headers.contentType = ContentType.json;
      req.contentLength = b.length;
      req.add(b);
    } else if (bytes != null) {
      req.headers.set(HttpHeaders.contentTypeHeader, contentType);
      req.contentLength = bytes.length;
      req.add(bytes);
    }
    final res = await req.close().timeout(timeout);
    // a certificate a public authority vouches for skips the callback: check the pin here too
    final cert = res.certificate;
    final fp = cert == null ? seen : fingerprintOf(cert);
    if (pin != null && fp != pin.toLowerCase()) {
      throw ServerError("Something answered at that address, but it isn't the server this phone was paired with.");
    }
    final body = await res.fold<List<int>>(<int>[], (a, b) => a..addAll(b)).timeout(timeout);
    return _Raw(res.statusCode, body, fp);
  } on ServerError {
    rethrow;
  } on HandshakeException {
    throw ServerError("Something answered at that address, but it isn't the server this phone was paired with.");
  } on TimeoutException {
    throw ServerError("The server didn't answer in time.");
  } on SocketException catch (e) {
    throw ServerError("The server can't be reached (${e.osError?.message ?? e.message}).");
  } on HttpException catch (e) {
    throw ServerError("The server can't be reached (${e.message}).");
  } finally {
    client.close(force: true);
  }
}

/// A first look at a server: is it FinGraph, and what's its certificate's fingerprint.
Future<String> probe(String address) async {
  final r = await _request(address, null, null, 'GET', '/api/v1/info');
  if (r.status != 200 || r.json is! Map || r.json['product'] != 'fingraph-server' || r.fingerprint == null) {
    throw ServerError("That isn't a FinGraph server.");
  }
  return r.fingerprint!;
}

class Server {
  Server({required this.address, required this.fingerprint, required this.token, required this.deviceId, required this.deviceName});
  final String address;
  final String fingerprint;
  final String token;
  final int deviceId;
  final String deviceName;

  Map<String, dynamic> toJson() => {'address': address, 'fingerprint': fingerprint, 'token': token, 'device_id': deviceId, 'device_name': deviceName};
  static Server fromJson(Map<String, dynamic> j) => Server(
      address: j['address'] as String,
      fingerprint: j['fingerprint'] as String,
      token: j['token'] as String,
      deviceId: (j['device_id'] as num?)?.toInt() ?? 0,
      deviceName: j['device_name'] as String? ?? 'Phone');

  static const _storage = FlutterSecureStorage();

  static Future<Server?> load() async {
    try {
      final s = await _storage.read(key: 'server');
      return s == null ? null : Server.fromJson(jsonDecode(s) as Map<String, dynamic>);
    } catch (_) {
      return null;
    }
  }

  Future<void> save() => _storage.write(key: 'server', value: jsonEncode(toJson()));
  static Future<void> forget() => _storage.delete(key: 'server');

  /// Pairs this phone using a code from `fingraph-server pair`. With a fingerprint (from the QR code) the
  /// server must have exactly that certificate.
  static Future<Server> pair(String address, String code, String? fingerprint, String deviceName) async {
    final fp = (fingerprint ?? await probe(address)).toLowerCase();
    final r = await _request(address, fp, null, 'POST', '/api/v1/pair',
        json: {'proof': pairingProof(code, fp), 'name': deviceName, 'platform': 'android'});
    if (r.status != 200) throw ServerError(r.error, status: r.status);
    final j = r.json as Map<String, dynamic>;
    final dev = j['device'] as Map<String, dynamic>;
    return Server(address: normalizeAddress(address), fingerprint: fp, token: j['token'] as String, deviceId: (dev['id'] as num).toInt(), deviceName: dev['name'] as String);
  }

  Future<dynamic> _get(String path) async {
    final r = await _request(address, fingerprint, token, 'GET', path);
    if (r.status != 200) throw ServerError(r.error, status: r.status);
    return r.json;
  }

  Future<Map<String, dynamic>> me() async => (await _get('/api/v1/me')) as Map<String, dynamic>;

  /// A screen's data (`summary`, `budget`, …).
  Future<Map<String, dynamic>> view(String name, Map<String, String> query) async {
    final q = query.isEmpty ? '' : '?${Uri(queryParameters: query).query}';
    return (await _get('/api/v1/view/$name$q')) as Map<String, dynamic>;
  }

  /// Sends a statement PDF to the server's inbox; returns the name it was queued under.
  Future<String> sendStatement(String name, List<int> bytes) async {
    final r = await _request(address, fingerprint, token, 'POST', '/api/v1/statements?${Uri(queryParameters: {'name': name}).query}',
        bytes: bytes, contentType: 'application/pdf', timeout: const Duration(seconds: 120));
    if (r.status != 200) throw ServerError(r.error, status: r.status);
    return (r.json as Map)['queued'] as String? ?? name;
  }

  /// A quick purchase, added to the ledger (or queued while someone is editing on a computer).
  Future<Map<String, dynamic>> addPurchase(Map<String, dynamic> purchase) async {
    final r = await _request(address, fingerprint, token, 'POST', '/api/v1/captures/purchase', json: purchase);
    if (r.status != 200) throw ServerError(r.error, status: r.status);
    return r.json as Map<String, dynamic>;
  }

  /// A receipt photo for a transaction.
  Future<Map<String, dynamic>> addReceipt(int transactionId, List<int> jpeg) async {
    final r = await _request(address, fingerprint, token, 'POST', '/api/v1/captures/receipt?transaction=$transactionId',
        bytes: jpeg, contentType: 'image/jpeg', timeout: const Duration(seconds: 120));
    if (r.status != 200) throw ServerError(r.error, status: r.status);
    return r.json as Map<String, dynamic>;
  }

  /// Unpairs this phone on the server (best effort) and forgets it here.
  Future<void> disconnect() async {
    try {
      await _request(address, fingerprint, token, 'DELETE', '/api/v1/devices/$deviceId');
    } catch (_) {}
    await forget();
  }
}
