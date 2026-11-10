import 'package:fingraph/src/connection.dart';
import 'package:fingraph/src/data.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('the pairing proof is the one the server checks', () {
    // sha256("fingraph-pair:ABCDEFGH:" + "ab" * 32), as crates/fingraph-server computes it
    expect(pairingProof('abcd-efgh', 'AB' * 32), 'c97a21d5542f6bcc5fe0cfcb39d373ab859edf4bd14dce8f4db07b3317f0d800');
  });

  test('addresses get the default port', () {
    expect(normalizeAddress('nas'), 'nas:8743');
    expect(normalizeAddress('https://192.168.1.20:9000/'), '192.168.1.20:9000');
    expect(normalizeAddress('[fd7a::1]:8743'), '[fd7a::1]:8743');
  });

  test('pairing QR codes', () {
    final l = PairingLink.parse('fingraph://pair?a=192.168.1.20:8743&c=ABCDEFGH&f=${'cd' * 32}')!;
    expect(l.address, '192.168.1.20:8743');
    expect(l.code, 'ABCDEFGH');
    expect(l.fingerprint, 'cd' * 32);
    expect(PairingLink.parse('https://example.com'), isNull);
    expect(PairingLink.parse('fingraph://pair?a=x'), isNull);
  });

  test('fingerprints are shown as the server prints them', () {
    expect(shortFingerprint('ab12' * 16), 'AB12 AB12 AB12 AB12');
  });

  test('money', () {
    expect(money('-23.4', r'$'), r'−$23.40');
    expect(money('1200', r'CI$'), r'CI$1,200.00');
    expect(money('5', '', sign: true), '+5.00');
  });
}
