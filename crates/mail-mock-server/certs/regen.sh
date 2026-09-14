#!/usr/bin/env bash
# Regenerates the TEST-ONLY TLS identity used by mail-mock-server.
#
# These certificates authenticate nothing real: they exist so integration
# tests can present a certificate chain for "localhost"/"127.0.0.1" that a
# CI job (or a developer) can install as an extra trusted root, letting
# esmail's real TLS code (imap.rs never disables verification) connect to
# the mock IMAP server without any changes to esmail itself.
#
# Run from this directory: ./regen.sh
# Then re-import ca.crt wherever it was trusted (see ../README.md).
set -euo pipefail
cd "$(dirname "$0")"

rm -f ca.key ca.crt ca.srl server.key server.crt server.csr server.ext server.p12

openssl genrsa -out ca.key 2048
openssl req -x509 -new -nodes -key ca.key -sha256 -days 3650 -out ca.crt -subj "/CN=esmail-test-ca"

openssl genrsa -out server.key 2048
openssl req -new -key server.key -out server.csr -subj "/CN=localhost"
printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n" > server.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out server.crt -days 3650 -sha256 -extfile server.ext

# PKCS#12 bundle is what native-tls's server-side Identity loader wants.
openssl pkcs12 -export -out server.p12 -inkey server.key -in server.crt -certfile ca.crt -passout pass:esmail-test-only

rm -f server.csr server.ext ca.srl ca.key
echo "Regenerated ca.crt, server.crt, server.key, server.p12 (ca.key discarded -- not needed at runtime)."
