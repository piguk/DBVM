# The template for this pack: python3 (with sqlite3) and the basic commands. Build and import it on a host with
# network access:
#   docker build -t selfdb-python packs/<pack> && docker save selfdb-python -o python.tar
#   self env-admin --data /srv/env image python python.tar --oci
# Without network access: packs/packkit/host-rootfs.sh python.tar python3 (import it without --oci).
FROM alpine:latest
RUN apk add --no-cache python3 bash coreutils grep sed findutils diffutils
