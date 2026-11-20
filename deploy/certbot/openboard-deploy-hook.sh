#!/bin/sh
# A certbot deploy hook for an Openboard board.
#
# certbot keeps certificates where only root can read them. The board does
# not run as root, so after each issuance and renewal this copies the
# certificate and its key to where the board's group can read them. The
# board notices the new files within seconds and serves the new certificate
# to new connections. Nothing is restarted and no open connection ends.
#
# Install as /usr/local/lib/openboard/certbot-deploy-hook and give it to
# certbot once, when the certificate is first obtained:
#
#   certbot certonly --standalone -d board.example \
#       --deploy-hook /usr/local/lib/openboard/certbot-deploy-hook
#
# certbot remembers the hook and runs it again after every renewal, with
# RENEWED_LINEAGE set to the directory that holds the new files.
#
# OPENBOARD_TLS_DIR and OPENBOARD_GROUP change where the copies go and
# which group may read the key.

set -eu

dest="${OPENBOARD_TLS_DIR:-/etc/openboard/tls}"
group="${OPENBOARD_GROUP:-openboard}"

if [ -z "${RENEWED_LINEAGE:-}" ]; then
    echo "certbot-deploy-hook: RENEWED_LINEAGE is not set; certbot runs this hook" >&2
    exit 1
fi

umask 077
install -d -m 0750 -o root -g "$group" "$dest"
# Each file is written beside its place and renamed into it, so the board
# never reads half a file. The key goes first. Between the two renames the
# board sees a new key with the old certificate, keeps serving the old pair,
# and picks up the new one when the certificate lands.
install -m 0640 -o root -g "$group" "$RENEWED_LINEAGE/privkey.pem" "$dest/privkey.pem.new"
install -m 0644 -o root -g "$group" "$RENEWED_LINEAGE/fullchain.pem" "$dest/fullchain.pem.new"
mv -f "$dest/privkey.pem.new" "$dest/privkey.pem"
mv -f "$dest/fullchain.pem.new" "$dest/fullchain.pem"
