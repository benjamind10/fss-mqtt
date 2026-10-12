#!/bin/sh
# Writes passphrase-protected private keys in every form OpenSSL produces, plus
# the plain DER of each key, for the key-loading tests. Passphrase: s3cret.
# Regenerate: docker run --rm -v "$PWD":/w -w /w alpine:3.20 sh -c "apk add -q openssl && sh make_keys.sh"
set -eu
P=pass:s3cret
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:1024 -out rsa.key 2>/dev/null
openssl pkcs8 -topk8 -nocrypt -in rsa.key -outform DER -out rsa-pkcs8.der
openssl rsa -in rsa.key -traditional -outform DER -out rsa-pkcs1.der 2>/dev/null
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ec.key
openssl pkcs8 -topk8 -nocrypt -in ec.key -outform DER -out ec-pkcs8.der
openssl ec -in ec.key -outform DER -out ec-sec1.der 2>/dev/null

# PKCS#8 / PBES2: OpenSSL 3's default (PBKDF2-HMAC-SHA256, AES-256-CBC) and variants.
openssl pkcs8 -topk8 -in rsa.key -v2 aes-256-cbc -passout $P -out rsa-pkcs8-aes256.key
openssl pkcs8 -topk8 -in rsa.key -v2 aes-128-cbc -v2prf hmacWithSHA1 -passout $P -out rsa-pkcs8-aes128-sha1.key
openssl pkcs8 -topk8 -in rsa.key -v2 aes-192-cbc -v2prf hmacWithSHA512 -passout $P -out rsa-pkcs8-aes192-sha512.key
openssl pkcs8 -topk8 -in rsa.key -v2 des3 -passout $P -out rsa-pkcs8-des3.key
openssl pkcs8 -topk8 -in ec.key -v2 aes-256-cbc -passout $P -out ec-pkcs8-aes256.key
# Unsupported on purpose: scrypt and the old PKCS#5 v1 scheme.
openssl pkcs8 -topk8 -in rsa.key -scrypt -passout $P -out rsa-pkcs8-scrypt.key
openssl pkcs8 -topk8 -in rsa.key -v1 PBE-SHA1-3DES -passout $P -out rsa-pkcs8-v1.key

# The older OpenSSL form: Proc-Type/DEK-Info headers.
openssl rsa -in rsa.key -traditional -aes256 -passout $P -out rsa-legacy-aes256.key 2>/dev/null
openssl rsa -in rsa.key -traditional -des3 -passout $P -out rsa-legacy-des3.key 2>/dev/null
openssl ec -in ec.key -aes128 -passout $P -out ec-legacy-aes128.key 2>/dev/null
ls -l
