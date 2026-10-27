#!/bin/sh
# Ensure Worktable's dedicated self-signed code-signing identity exists.
#
# Releases are signed with this identity by default so the designated
# requirement is anchored to a stable certificate leaf instead of an ad-hoc
# cdhash. The certificate and private key live in their own keychain under
# ~/.worktable/codesign; nothing is created in or trusted from the login
# keychain.
#
# Prints only the keychain path on stdout. All progress and errors go to
# stderr, so callers can use `keychain=$(scripts/ensure-self-signed-identity.sh)`.
set -eu

IDENTITY="Worktable Self-Signed Code Signing"
CODESIGN_DIR=${WORKTABLE_CODESIGN_DIR:-"$HOME/.worktable/codesign"}
KEYCHAIN="$CODESIGN_DIR/worktable-codesign.keychain-db"
PASSWORD_FILE="$CODESIGN_DIR/password"
CERT_PEM="$CODESIGN_DIR/cert.pem"
CERT_DAYS=3650

log() {
  printf '%s\n' "$*" >&2
}

if ! command -v openssl >/dev/null 2>&1; then
  log "openssl is required to create the Worktable code-signing identity."
  log "Install OpenSSL 3 (for example: brew install openssl@3) and try again."
  exit 1
fi

# macOS `security` rejects modern PKCS#12 encryption, so this script must
# export with OpenSSL's legacy provider.
if ! openssl pkcs12 -help 2>&1 | grep -q -- '-legacy'; then
  log "This openssl does not support 'pkcs12 -legacy', which macOS security needs."
  log "Install OpenSSL 3 and try again."
  exit 1
fi

umask 077
mkdir -p "$CODESIGN_DIR"
chmod 700 "$CODESIGN_DIR"

if [ -f "$KEYCHAIN" ] && [ ! -f "$PASSWORD_FILE" ]; then
  log "The codesigning keychain exists but its password file is missing:"
  log "  $PASSWORD_FILE"
  log "Remove $KEYCHAIN to create a new identity. That changes the designated requirement."
  exit 1
fi

if [ -f "$PASSWORD_FILE" ]; then
  PASSWORD=$(cat "$PASSWORD_FILE")
else
  PASSWORD=$(openssl rand -hex 32)
  printf '%s' "$PASSWORD" > "$PASSWORD_FILE"
  chmod 600 "$PASSWORD_FILE"
fi

# Reuse the existing identity when the keychain already has it. Recreating a
# certificate would change the designated requirement and may cost one
# Accessibility re-grant.
if [ -f "$KEYCHAIN" ]; then
  if ! security unlock-keychain -p "$PASSWORD" "$KEYCHAIN" >/dev/null 2>&1; then
    log "Couldn't unlock the Worktable codesigning keychain: $KEYCHAIN"
    log "Check the password in $PASSWORD_FILE, or remove the keychain to recreate the identity."
    exit 1
  fi
  if security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -F "$IDENTITY" >/dev/null; then
    printf '%s\n' "$KEYCHAIN"
    exit 0
  fi
  log "The Worktable codesigning keychain exists without '$IDENTITY'; creating the identity."
else
  log "Creating the Worktable codesigning keychain: $KEYCHAIN"
  security create-keychain -p "$PASSWORD" "$KEYCHAIN" >&2
fi

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT INT TERM
KEY_PEM="$WORK_DIR/key.pem"
CERT_TMP="$WORK_DIR/cert.pem"
P12="$WORK_DIR/identity.p12"
OPENSSL_CONFIG="$WORK_DIR/openssl.cnf"

cat > "$OPENSSL_CONFIG" <<EOF
[ req ]
distinguished_name = distinguished_name
prompt = no
default_md = sha256
x509_extensions = extensions

[ distinguished_name ]
CN = $IDENTITY

[ extensions ]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

log "Generating a 2048-bit RSA self-signed certificate for '$IDENTITY'."
openssl req -x509 -newkey rsa:2048 -nodes -days "$CERT_DAYS" \
  -keyout "$KEY_PEM" -out "$CERT_TMP" -config "$OPENSSL_CONFIG" >&2

# `security import` refuses OpenSSL's default PKCS#12 algorithms on macOS.
openssl pkcs12 -export -legacy \
  -inkey "$KEY_PEM" -in "$CERT_TMP" \
  -name "$IDENTITY" -out "$P12" \
  -passout "pass:$PASSWORD" >&2

if ! security import "$P12" \
  -k "$KEYCHAIN" \
  -P "$PASSWORD" \
  -T /usr/bin/codesign \
  -f pkcs12 >/dev/null; then
  log "Couldn't import the generated certificate into $KEYCHAIN."
  exit 1
fi

# Let codesign use the imported private key without an interactive prompt.
security set-key-partition-list \
  -S apple-tool:,apple:,codesign: \
  -s -k "$PASSWORD" \
  "$KEYCHAIN" >&2

cp "$CERT_TMP" "$CERT_PEM"
chmod 600 "$CERT_PEM"

# Trust only as a code-signing root in the user domain, and only for this
# dedicated keychain. The login keychain is never touched.
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$CERT_PEM" >&2

if ! security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -F "$IDENTITY" >/dev/null; then
  log "Failed to create a valid code-signing identity in $KEYCHAIN."
  exit 1
fi

printf '%s\n' "$KEYCHAIN"
