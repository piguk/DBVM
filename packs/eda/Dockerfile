# The EDA template's image: Verilator and ngspice with what the judges need (python3) and what Verilator builds
# with (g++, make, perl). Build and import it on a host with network access:
#
#   docker build -t selfdb-eda packs/eda && docker save selfdb-eda -o eda.tar
#   self env-admin --data /srv/env image eda eda.tar --oci
#
# Without network access, packs/eda/scripts/host-rootfs.sh assembles a root filesystem from a Debian or Ubuntu
# host's packages instead (import it without --oci).
FROM alpine:latest
RUN apk add --no-cache verilator ngspice g++ make perl python3 bash coreutils grep sed findutils diffutils
WORKDIR /work
