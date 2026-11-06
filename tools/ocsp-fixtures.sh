#!/bin/bash
# Generates the throwaway certificates of crates/core/tests/ocsp.rs (test tooling only: the
# openssl CLI is not part of the extension; the OCSP responses themselves are built and signed by
# the test): ocsp-fixtures.sh [dir] (default crates/core/tests/ocsp)
#   ca.pem / ca.key          test CA (RSA 2048)
#   leaf.pem / leaf.key      server certificate: SAN localhost, 127.0.0.1; OCSP responder
#                            http://127.0.0.1:28041/ (Authority Information Access)
#   staple.pem / staple.key  the same, marked "must staple" (TLS feature status_request)
#   noaia.pem / noaia.key    the same, without an OCSP responder
#   responder.pem / .key     delegated responder: issued by the CA, extended key usage OCSPSigning
#   noeku.pem / noeku.key    issued by the CA without OCSPSigning
#   other.pem / other.key    an unrelated self-signed CA
# Valid for 100 years. Keys are PKCS#8, unencrypted.
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
D=${1:-$ROOT/crates/core/tests/ocsp}
O=${OPENSSL:-openssl}
mkdir -p "$D" && cd "$D"
rm -f ./*.pem ./*.key ./*.csr ./*.srl ./*.cnf
days=36500
key() { $O genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$1.key" 2>/dev/null; }
key ca; $O req -x509 -new -key ca.key -out ca.pem -days $days -subj "/CN=mdrop OCSP test CA/O=mdrop" 2>/dev/null
key other; $O req -x509 -new -key other.key -out other.pem -days $days -subj "/CN=mdrop OCSP test CA/O=elsewhere" 2>/dev/null
leaf() { # name subject extensions...
  n=$1; s=$2; shift 2
  printf '%s\n' "$@" > "ext-$n.cnf"
  key "$n"
  $O req -new -key "$n.key" -out "$n.csr" -subj "$s" 2>/dev/null
  $O x509 -req -in "$n.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -out "$n.pem" -days $days -extfile "ext-$n.cnf" 2>/dev/null
}
srv="basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1"
leaf leaf "/CN=leaf/O=mdrop" "$srv" "authorityInfoAccess=OCSP;URI:http://127.0.0.1:28041/"
leaf staple "/CN=staple/O=mdrop" "$srv" "authorityInfoAccess=OCSP;URI:http://127.0.0.1:28041/" "tlsfeature=status_request"
leaf noaia "/CN=noaia/O=mdrop" "$srv"
leaf responder "/CN=responder/O=mdrop" "basicConstraints=CA:FALSE" "keyUsage=digitalSignature" "extendedKeyUsage=OCSPSigning"
leaf noeku "/CN=noeku/O=mdrop" "basicConstraints=CA:FALSE" "keyUsage=digitalSignature" "extendedKeyUsage=serverAuth"
rm -f ./*.csr ./*.srl ./*.cnf
