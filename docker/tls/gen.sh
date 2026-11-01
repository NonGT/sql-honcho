#!/usr/bin/env bash
# Test-only CA, server and client certs for the TLS tests. The server cert
# covers db.honcho.test and 127.0.0.1 but not localhost, so verify-full can
# be seen failing on a name. Rerun to regenerate; valid for ten years.
set -euo pipefail
cd "$(dirname "$0")"
days=3650

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out ca-key.pem
openssl req -x509 -new -key ca-key.pem -days $days -subj "/CN=sql-honcho test CA" -out ca.pem

issue() {
  local name=$1 subj=$2 ext=$3
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$name-key.pem"
  openssl req -new -key "$name-key.pem" -subj "$subj" -out "$name.csr"
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
    -days $days -extfile <(printf '%s' "$ext") -out "$name-cert.pem"
  rm "$name.csr"
}

issue server "/CN=db.honcho.test" "subjectAltName=DNS:db.honcho.test,IP:127.0.0.1
basicConstraints=CA:FALSE
extendedKeyUsage=serverAuth"
issue client "/CN=honcho_x509" "basicConstraints=CA:FALSE
extendedKeyUsage=clientAuth"

rm -f ca.srl
# the servers run as their own users and read these from a read-only mount
chmod 644 ./*.pem
