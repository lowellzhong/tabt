#!/bin/bash
# Create (once per machine) a local self-signed "TabT Dev" code-signing certificate in the
# login keychain, so `make run`/`make` can sign TabT.app with a *stable* identity.
#
# Why this matters: `codesign --sign -` (ad-hoc) derives the app's designated requirement from
# a hash of the binary itself, so it changes on every rebuild. macOS TCC keys folder-access
# grants (Desktop/Documents/Downloads/etc.) off that requirement, so an ad-hoc-signed app gets
# treated as "a new app" on every rebuild and re-prompts. A certificate-backed signature's
# designated requirement instead pins to the certificate (`identifier "..." and certificate
# leaf = H"..."`), which stays identical across rebuilds as long as the same certificate is
# reused -- so TCC grants persist. No keychain "trust" step is required for codesign itself to
# use the certificate; `-T /usr/bin/codesign` is enough.
set -euo pipefail

CERT_NAME="TabT Dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-certificate -c "$CERT_NAME" "$KEYCHAIN" >/dev/null 2>&1; then
    echo "==> '$CERT_NAME' already exists in the login keychain, nothing to do"
    exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/cert.conf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $CERT_NAME
[ext]
basicConstraints = critical,CA:true
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

openssl req -x509 -newkey rsa:2048 -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
    -days 3650 -nodes -config "$TMP/cert.conf"
openssl pkcs12 -export -out "$TMP/cert.p12" -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -passout pass:tabt

security import "$TMP/cert.p12" -k "$KEYCHAIN" -P tabt -T /usr/bin/codesign

echo "==> created '$CERT_NAME' in the login keychain"
