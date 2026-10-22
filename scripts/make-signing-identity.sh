#!/usr/bin/env bash
# Create a stable, self-signed code-signing identity "Darkslide Dev" in the login
# keychain, so every build has the same designated requirement and macOS privacy
# (TCC) grants survive rebuilds. Ad-hoc signatures change the cdhash per build,
# which silently drops grants such as Removable Volumes and Local Network.
#
# Development only: this is a self-signed identity trusted for code signing on this Mac
# alone. It is not a distribution certificate (see docs/RELEASING.md for that).
#
# Run once, interactively (macOS may ask for your login keychain password):
#   scripts/make-signing-identity.sh
# build-app.sh picks the identity up automatically.
set -euo pipefail
NAME="Darkslide Dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -p codesigning "$KEYCHAIN" | grep -q "\"$NAME\""; then
  echo "\"$NAME\" already exists:"
  security find-identity -p codesigning "$KEYCHAIN" | grep "\"$NAME\""
  exit 0
fi

WORK="$(mktemp -d)"
trap '/bin/rm -rf "$WORK"' EXIT
cat >"$WORK/openssl.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 730 -config "$WORK/openssl.cnf" \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null
# The PKCS#12 password reaches openssl through the environment, not argv. (`security import`
# below has no such option; the password only guards this throwaway file in $WORK.)
PASS="$(/usr/bin/openssl rand -hex 16)"
export PASS
/usr/bin/openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -name "$NAME" \
  -passout "env:PASS" -out "$WORK/id.p12" -legacy 2>/dev/null \
  || /usr/bin/openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -name "$NAME" \
       -passout "env:PASS" -out "$WORK/id.p12"

# -T: let codesign use the key without a prompt on every build.
security import "$WORK/id.p12" -k "$KEYCHAIN" -P "$PASS" -T /usr/bin/codesign
# Trust the certificate for code signing (asks for your password).
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$WORK/cert.pem"

echo "Created \"$NAME\":"
security find-identity -p codesigning "$KEYCHAIN" | grep "\"$NAME\"" || true
echo "Rebuild with scripts/build-app.sh; the first launch re-asks for permissions once, then they stick."
