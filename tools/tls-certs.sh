#!/bin/bash
# Generates throwaway TLS test certificates (test tooling only: the openssl CLI is not part of the
# extension): tls-certs.sh <dir>
#   ca.pem             test CA (RSA 2048, 10 days)
#   other-ca.pem       an unrelated CA (for verification failures)
#   server.pem         server certificate + key: CN=server, SAN localhost, 127.0.0.1 (for mongod)
#   server-ip.pem      server certificate for 127.0.0.2 only (host name mismatch on 127.0.0.1)
#   client.pem         client certificate + PKCS#8 key: CN=client,OU=drivers,O=mdrop-client in RFC 2253 order, the user
#                      name of MONGODB-X509 (its O differs from
#                      the server certificate's, else mongod treats it as a cluster member)
#   client-pkcs8-enc.pem  the same, key as encrypted PKCS#8 (password qwerty)
#   client-legacy-enc.pem the same, key with OpenSSL's legacy PEM encryption (AES-256-CBC, qwerty)
#   crl-empty.pem      a CRL of ca.pem revoking nothing
#   crl-revoked.pem    a CRL of ca.pem revoking server.pem
set -e
D=${1:?dir}
mkdir -p "$D" && cd "$D"
rm -f ./*.pem ./*.key ./*.csr ./*.srl index.txt* crlnumber* ca.cnf ext-*.cnf
days=10
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days $days -subj "/CN=mdrop test CA/O=mdrop" 2>/dev/null
cp ca.crt ca.pem
openssl req -x509 -newkey rsa:2048 -nodes -keyout other-ca.key -out other-ca.pem -days $days -subj "/CN=other CA/O=elsewhere" 2>/dev/null
leaf() { # name subject san
  printf 'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth,clientAuth\n%s\n' "$3" > "ext-$1.cnf"
  openssl req -newkey rsa:2048 -nodes -keyout "$1.key" -out "$1.csr" -subj "$2" 2>/dev/null
  openssl x509 -req -in "$1.csr" -CA ca.crt -CAkey ca.key -CAcreateserial -out "$1.crt" -days $days -extfile "ext-$1.cnf" 2>/dev/null
}
leaf server "/CN=server/O=mdrop" "subjectAltName=DNS:localhost,IP:127.0.0.1"
leaf server-ip "/CN=server-ip/O=mdrop" "subjectAltName=IP:127.0.0.2"
leaf client "/O=mdrop-client/OU=drivers/CN=client" ""
cat server.crt server.key > server.pem
cat server-ip.crt server-ip.key > server-ip.pem
openssl pkcs8 -topk8 -nocrypt -in client.key -out client.p8 2>/dev/null
cat client.crt client.p8 > client.pem
openssl pkcs8 -topk8 -v2 aes-256-cbc -passout pass:qwerty -in client.key -out client.p8e 2>/dev/null
cat client.crt client.p8e > client-pkcs8-enc.pem
# -traditional exists in OpenSSL 3 only; LibreSSL writes the legacy format by default
openssl rsa -in client.key -aes256 -passout pass:qwerty -traditional -out client.lenc 2>/dev/null \
  || openssl rsa -in client.key -aes256 -passout pass:qwerty -out client.lenc 2>/dev/null
grep -q "Proc-Type: 4,ENCRYPTED" client.lenc
cat client.crt client.lenc > client-legacy-enc.pem
# CRLs (openssl ca needs a small database)
touch index.txt; echo 01 > crlnumber
cat > ca.cnf <<EOF
[ca]
default_ca = d
[d]
database = index.txt
crlnumber = crlnumber
default_md = sha256
default_crl_days = $days
certificate = ca.crt
private_key = ca.key
EOF
openssl ca -config ca.cnf -gencrl -out crl-empty.pem 2>/dev/null
openssl ca -config ca.cnf -revoke server.crt 2>/dev/null
openssl ca -config ca.cnf -gencrl -out crl-revoked.pem 2>/dev/null
rm -f ./*.csr ./*.srl ./*.p8 ./*.p8e ./*.lenc ext-*.cnf ca.cnf index.txt* crlnumber*
ls
# a CA directory as OpenSSL looks it up (hash-named files), for the ca_dir driver option
mkdir -p hashed && cp ca.pem "hashed/$(openssl x509 -hash -noout -in ca.pem).0"
