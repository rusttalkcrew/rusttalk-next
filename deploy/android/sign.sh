#!/usr/bin/env bash
# The protected release job supplies the PKCS12 and its password.
set -euo pipefail
: "${RUSTTALK_ANDROID_KEYSTORE:?Missing Android release keystore}"
: "${RUSTTALK_ANDROID_KEYSTORE_PASSWORD:?Missing Android release password}"
umask 077
key=$(mktemp)
trap 'rm -f "$key"' EXIT
printf %s "$RUSTTALK_ANDROID_KEYSTORE" | base64 --decode > "$key"
signer=$ANDROID_HOME/build-tools/36.0.0/apksigner
"$signer" sign --ks "$key" --ks-type PKCS12 --ks-key-alias rusttalk \
  --ks-pass env:RUSTTALK_ANDROID_KEYSTORE_PASSWORD --key-pass env:RUSTTALK_ANDROID_KEYSTORE_PASSWORD \
  --v4-signing-enabled false --out "$2" "$1"
"$signer" verify "$2"
