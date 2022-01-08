#!/bin/sh

set -e

echo "👷 Setting up Conduit instance '${SERVER_NAME}' to be tested with Complement..."

# We ecpect the following files to be mounted into the container:
# /complement/ca/ca.crt
# /complement/ca/ca.key


printf "\n👷 Generating certificate signing request (csr) for the complement dummy CA"
openssl req -new -sha256 \
  -key "/conduit-https.key" \
  -subj "/C=US/ST=CA/O=ComplementOrg, Inc./CN=${SERVER_NAME}" \
  -out "${SERVER_NAME}.csr"

printf "\n👷 Signing the homeserver's cert with the complement dummy CA"
openssl x509 -req -sha256 -days 2 \
  -in "${SERVER_NAME}.csr" \
  -CA /complement/ca/ca.crt \
  -CAkey /complement/ca/ca.key \
  -CAcreateserial \
  -out "${SERVER_NAME}.crt" \

printf "\n👷 Packing https cert+key and CA cert into a PEM file for Caddy (http reverse proxy) to read"
cat "/conduit-https.key"  >> /conduit.complement.key.pem
cat "${SERVER_NAME}.crt"  >> /conduit.complement.crt.pem
#cat /complement/ca/ca.key >> /conduit.complement.key.pem
cat /complement/ca/ca.crt >> /conduit.complement.crt.pem

printf "\n👷 Updating the OS CA trust store"
cp /complement/ca/ca.crt /usr/local/share/ca-certificates/
update-ca-certificates || true

export CONDUIT_SERVER_NAME="${SERVER_NAME}"

printf "\n👷 Configuring Caddy to listen on 'http(s)://%s'" "${SERVER_NAME}"
sed -i "s/your.server.name/${SERVER_NAME}/g" /complement-caddy.json 
(caddy start --config /complement-caddy.json) >> /tmp/caddy.log 2>> /tmp/caddy.err.log

TMP_DB_DIR="$(mktemp -d -p '/tmp' 'conduit_db_dir_XXXXXXXXXX')"
printf "\n👷 Preparing '%s' as Conduit's database directory" "${TMP_DB_DIR}"
rm -rf "$TMP_DB_DIR" || true
mkdir -p "$TMP_DB_DIR"
export CONDUIT_CONDUIT_DATABASE_PATH="${DB_DIR}"

printf "\n👷 Starting Conduit with address '%s'\n\n" "${SERVER_NAME}"
/srv/conduit/conduit
