#!/bin/sh
# The EDA template with YoWASP (Yosys, nextpnr-ice40) at /opt/yowasp, from this host's packages: for the fpga tasks.
#
#   YOWASP=DIR packs/eda/fpga-template.sh OUT.tar
#
# DIR is where `pip install --target DIR yowasp-yosys yowasp-nextpnr-ice40` put them. Import OUT.tar as a template,
# then warm YoWASP's compile cache into a frozen template: python3 packs/eda/warm_fpga.py BASE_URL TEMPLATE_ID.
set -eu
out=${1:?usage: YOWASP=DIR fpga-template.sh OUT.tar}
yowasp=${YOWASP:?set YOWASP to the directory yowasp-yosys and yowasp-nextpnr-ice40 are installed in}
here=$(cd "$(dirname "$0")" && pwd)
"$here/../packkit/host-rootfs.sh" "$out" verilator ngspice g++ make perl python3
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/opt"
cp -a "$yowasp" "$stage/opt/yowasp"
tar -rf "$out" --owner=0 --group=0 --numeric-owner -C "$stage" opt
echo "$out: the EDA packages and YoWASP" >&2
