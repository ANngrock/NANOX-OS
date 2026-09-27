#!/usr/bin/env bash
# Read-only hardware inventory of a machine booted into a live Linux, for
# NANOX M9 (docs/hardware/README.md). Offline; needs bash, coreutils, awk,
# sed, grep and, for a full capture, root, pciutils (lspci) and dmidecode.
#
#   sudo ./collect-linux.sh OUT_DIR          full capture; fails if anything is missing
#   ./collect-linux.sh --partial OUT_DIR     best effort; records what was skipped
#
# Writes only inside OUT_DIR (which must not exist or be empty):
#   inventory.json      input for profile.py (schema nanox-hw-inventory v1)
#   collect.log         status of every part
#   cpuinfo.txt         first /proc/cpuinfo record and the processor count
#   lspci-nnvvv.txt     lspci -nnvvv, Device Serial Number lines removed
#   lspci-xxxx.txt      lspci -nnxxxx, DSN capability payload zeroed
#   dmidecode.txt       dmidecode without serials/UUID/asset tags/OEM records
#   dmesg-iommu.txt     dmesg lines about IOMMU/AMD-Vi/DMAR/x2APIC/SMP
#   iommu-groups.txt    IOMMU units and groups from sysfs
#   acpi/SIG.bin        allowlisted ACPI tables only, acpi/SHA256SUMS
#
# Never collected: MSDM (product key), VFCT (VBIOS), DSDT/SSDT AML, MAC
# addresses, serial numbers, UUIDs, host and user names. Before finishing,
# every file is searched for the real values of those (read into memory
# only); text hits are replaced by <redacted>, any remaining hit is an error.
#
# Internal filter used by the tests: --filter-config-space (stdin -> stdout).

set -euo pipefail
umask 077

ALLOWED_ACPI="APIC FACP HPET MCFG IVRS DMAR SRAT SLIT"
COLLECTOR_VERSION=1

die() { echo "collect-linux: error: $*" >&2; exit 1; }
usage() { echo "usage: $0 [--partial] OUT_DIR" >&2; exit 2; }

# Zero the payload of PCIe extended capability 0003h (Device Serial Number)
# in `lspci -xxxx` output. Portable awk (no gawk bit functions).
filter_config_space() {
    awk '
    function hexval(h,   i, v, c) {
        v = 0; h = tolower(h)
        for (i = 1; i <= length(h); i++) {
            c = index("0123456789abcdef", substr(h, i, 1))
            if (c == 0) return -1
            v = v * 16 + c - 1
        }
        return v
    }
    function flush(   off, guard, id, nxt, i, k, s) {
        off = 256; guard = 0
        while (off >= 256 && off + 4 <= nb && guard++ < 1024) {
            id = b[off] + b[off + 1] * 256
            nxt = int(b[off + 2] / 16) + b[off + 3] * 16
            if (id == 0 || id == 65535) break
            if (id == 3) for (i = off + 4; i < off + 12 && i < nb; i++) b[i] = 0
            nxt -= nxt % 4
            if (nxt < 256) break
            off = nxt
        }
        for (k = 1; k <= n; k++) {
            s = label[k] ":"
            for (i = 0; i < cnt[k]; i++) s = s sprintf(" %02x", b[start[k] + i])
            print s
        }
        n = 0; nb = 0; split("", b); split("", label); split("", start); split("", cnt)
    }
    /^[0-9a-f]+: [0-9a-f][0-9a-f]( [0-9a-f][0-9a-f])*$/ {
        n++; label[n] = substr($1, 1, length($1) - 1); start[n] = hexval(label[n]); cnt[n] = NF - 1
        for (i = 2; i <= NF; i++) b[start[n] + i - 2] = hexval($i)
        if (start[n] + NF - 1 > nb) nb = start[n] + NF - 1
        next
    }
    { flush(); print }
    END { flush() }
    '
}

# Remove MAC- and UUID-shaped strings and Device Serial Number lines.
scrub_text() {
    sed -E \
        -e '/[Ss]erial [Nn]umber [0-9a-fA-F]{2}-/d' \
        -e 's/([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}/<mac>/g' \
        -e 's/[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}/<uuid>/g'
}

# dmidecode without identifying values and without raw OEM-specific records.
scrub_dmidecode() {
    awk '
    /^Handle / { skip = 0 }
    /^OEM-specific Type/ { skip = 1 }
    skip { next }
    {
        if (match($0, /^[ \t]*(Serial Number|SerialNumber|UUID|Asset Tag|Service Tag|Chassis Handle Serial)[ \t]*:/)) {
            print substr($0, 1, RLENGTH) " <redacted>"; next
        }
        print
    }' | scrub_text
}

if [ "${1:-}" = "--filter-config-space" ]; then
    filter_config_space
    exit 0
fi

PARTIAL=0
if [ "${1:-}" = "--partial" ]; then PARTIAL=1; shift; fi
[ $# -eq 1 ] || usage
OUT=$1

for tool in awk sed grep sha256sum base64 od tr sort head tail wc readlink cat cut paste find date uname id basename cp mv ls; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool '$tool' not found"
done
missing=""
for tool in lspci dmidecode dmesg; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
IS_ROOT=0
[ "$(id -u)" -eq 0 ] && IS_ROOT=1
if [ $PARTIAL -eq 0 ]; then
    [ -z "$missing" ] || die "missing tools:$missing (install pciutils/dmidecode or use --partial)"
    [ $IS_ROOT -eq 1 ] || die "a full capture needs root (ACPI tables, dmidecode, extended PCI config space); use sudo or --partial"
fi

if [ -e "$OUT" ]; then
    [ -d "$OUT" ] || die "$OUT exists and is not a directory"
    [ -z "$(ls -A "$OUT")" ] || die "$OUT is not empty"
fi
mkdir -p "$OUT/acpi"
LOG="$OUT/collect.log"
: >"$LOG"
LIMITS=()
note() { echo "$1: $2" >>"$LOG"; }
skip() {
    note "$1" "skipped: $2"
    LIMITS+=("$1: $2")
    [ $PARTIAL -eq 1 ] || die "$1: $2"
}
have() { command -v "$1" >/dev/null 2>&1; }
rd() { if [ -r "$1" ]; then tr -d '\000' <"$1" | head -c 256 | tr -d '\n'; fi; }

# JSON helpers: strings are escaped, control characters dropped, empty -> null.
jstr() {
    local s
    s=$(printf '%s' "$1" | tr -d '\000-\010\013-\037' | tr '\t\n\r' '   ' | sed -e 's/^ *//' -e 's/ *$//')
    if [ -z "$s" ]; then printf 'null'; return; fi
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    printf '"%s"' "$s"
}
jnum() { if [[ "${1:-}" =~ ^[0-9]+$ ]]; then printf '%s' "$1"; else printf 'null'; fi; }
join_by_comma() { local IFS=,; printf '%s' "$*"; }

# ------------------------------------------------------------------ CPU
awk 'BEGIN { RS = "" } NR == 1 { print } END { print "processors: " NR }' /proc/cpuinfo >"$OUT/cpuinfo.txt"
cpu_field() { awk -F': *' -v k="$1" '$1 ~ "^"k"[ \t]*$" { print $2; exit }' /proc/cpuinfo; }
threads=$(grep -c '^processor' /proc/cpuinfo || true)
packages=$(awk -F': *' '/^physical id/ { print $2 }' /proc/cpuinfo | sort -u | wc -l)
cores=$(awk -F': *' '/^physical id/ { p = $2 } /^core id/ { print p ":" $2 }' /proc/cpuinfo | sort -u | wc -l)
[ "$packages" -gt 0 ] || packages=""
[ "$cores" -gt 0 ] || cores=""
max_khz=$(rd /sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq)
max_mhz=""; [[ "$max_khz" =~ ^[0-9]+$ ]] && max_mhz=$((max_khz / 1000))
virt=false; grep -qwE 'svm|vmx' /proc/cpuinfo && virt=true
cpu_json=$(printf '{"name":%s,"vendor":%s,"family":%s,"model":%s,"stepping":%s,"packages":%s,"cores":%s,"threads":%s,"max_mhz":%s,"l2_kib":null,"l3_kib":null,"virtualization_enabled":%s}' \
    "$(jstr "$(cpu_field 'model name')")" "$(jstr "$(cpu_field vendor_id)")" \
    "$(jnum "$(cpu_field 'cpu family')")" "$(jnum "$(cpu_field model)")" "$(jnum "$(cpu_field stepping)")" \
    "$(jnum "$packages")" "$(jnum "$cores")" "$(jnum "$threads")" "$(jnum "$max_mhz")" "$virt")
note cpu ok

# ------------------------------------------------------ system and BIOS
D=/sys/class/dmi/id
if [ -d "$D" ]; then note dmi-sysfs ok; else skip dmi-sysfs "no $D"; fi
bios_date=$(rd "$D/bios_date")
if [[ "$bios_date" =~ ^([0-9]{2})/([0-9]{2})/([0-9]{4})$ ]]; then
    bios_date="${BASH_REMATCH[3]}-${BASH_REMATCH[1]}-${BASH_REMATCH[2]}"
fi
system_json=$(printf '{"manufacturer":%s,"model":%s,"family":%s,"version":%s,"sku":%s,"board_manufacturer":%s,"board_product":%s,"board_version":%s,"hypervisor_present":%s}' \
    "$(jstr "$(rd "$D/sys_vendor")")" "$(jstr "$(rd "$D/product_name")")" "$(jstr "$(rd "$D/product_family")")" \
    "$(jstr "$(rd "$D/product_version")")" "$(jstr "$(rd "$D/product_sku")")" "$(jstr "$(rd "$D/board_vendor")")" \
    "$(jstr "$(rd "$D/board_name")")" "$(jstr "$(rd "$D/board_version")")" \
    "$(if grep -qw hypervisor /proc/cpuinfo; then echo true; else echo false; fi)")
bios_json=$(printf '{"vendor":%s,"version":%s,"date":%s,"smbios_version":null,"release":%s,"ec_release":%s}' \
    "$(jstr "$(rd "$D/bios_vendor")")" "$(jstr "$(rd "$D/bios_version")")" "$(jstr "$bios_date")" \
    "$(jstr "$(rd "$D/bios_release")")" "$(jstr "$(rd "$D/ec_firmware_release")")")

# ------------------------------------------------------------- firmware
fw_type=legacy; [ -d /sys/firmware/efi ] && fw_type=uefi
secure_boot=unknown
sbvar=/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-e38d2b8c1d44
if [ -r "$sbvar" ]; then
    case "$(od -An -t u1 -j4 -N1 "$sbvar" | tr -d ' ')" in
        1) secure_boot=enabled ;;
        0) secure_boot=disabled ;;
    esac
fi
firmware_json=$(printf '{"type":"%s","secure_boot":"%s","dma_protection_available":null}' "$fw_type" "$secure_boot")

# --------------------------------------------------------------- memory
mem_kib=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo)
installed=""
modules_json="[]"
if [ $IS_ROOT -eq 1 ] && have dmidecode; then
    dmidecode 2>/dev/null | scrub_dmidecode >"$OUT/dmidecode.txt"
    note dmidecode ok
    mod_lines=$(dmidecode -t 17 2>/dev/null | awk '
        function esc(s) { gsub(/\\/, "\\\\", s); gsub(/"/, "\\\"", s); sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
        function str(s) { s = esc(s); return (s == "" || s ~ /^(Unknown|Not Specified|None)$/) ? "null" : "\"" s "\"" }
        function num(s) { return (s ~ /^[0-9]+$/) ? s : "null" }
        function emit() {
            if (!inrec || size == "") return
            printf "%s {\"locator\":%s,\"bank\":%s,\"size_bytes\":%s,\"speed_mts\":%s,\"configured_mts\":%s,\"manufacturer\":%s,\"part_number\":%s,\"smbios_type\":null}\n", size, str(loc), str(bank), size, num(sp), num(csp), str(man), str(part)
        }
        /^Memory Device/ { emit(); inrec = 1; size = ""; loc = bank = man = part = sp = csp = ""; next }
        /^Handle / { emit(); inrec = 0; next }
        inrec && /^\tSize: [0-9]+ (MB|GB|TB)/ { split($0, a, " "); m = (a[3] == "MB") ? 1048576 : (a[3] == "GB") ? 1073741824 : 1099511627776; size = sprintf("%.0f", a[2] * m) }
        inrec && /^\tLocator:/ { loc = substr($0, index($0, ":") + 1) }
        inrec && /^\tBank Locator:/ { bank = substr($0, index($0, ":") + 1) }
        inrec && /^\tManufacturer:/ { man = substr($0, index($0, ":") + 1) }
        inrec && /^\tPart Number:/ { part = substr($0, index($0, ":") + 1) }
        inrec && /^\tSpeed: [0-9]+/ { split($0, a, " "); sp = a[2] }
        inrec && /^\tConfigured Memory Speed: [0-9]+/ { split($0, a, " "); csp = a[4] }
        END { emit() }')
    if [ -n "$mod_lines" ]; then
        installed=$(printf '%s\n' "$mod_lines" | awk '{ s += $1 } END { printf "%.0f", s }')
        modules_json="[$(printf '%s\n' "$mod_lines" | cut -d' ' -f2- | paste -sd, -)]"
    fi
else
    skip dmidecode "needs root and dmidecode; memory modules unknown"
fi
memory_json=$(printf '{"installed_bytes":%s,"visible_bytes":%s,"modules":%s}' \
    "$(jnum "$installed")" "$(jnum "$((mem_kib * 1024))")" "$modules_json")

# ------------------------------------------------------------------ PCI
if have lspci; then
    lspci -nnvvv 2>/dev/null | scrub_text >"$OUT/lspci-nnvvv.txt"
    lspci -nnxxxx 2>/dev/null | filter_config_space | scrub_text >"$OUT/lspci-xxxx.txt"
    if [ $IS_ROOT -eq 1 ]; then note lspci ok; else skip lspci-config "not root: capabilities and config space beyond 64 bytes are hidden"; fi
else
    skip lspci "lspci not found; device names and config space not captured"
fi
hx() { local v; v=$(rd "$1"); v=${v#0x}; printf '%s' "$v"; }
pci_items=()
for dev in /sys/bus/pci/devices/*; do
    [ -e "$dev/vendor" ] || continue
    bdf=${dev##*/}
    class=$(hx "$dev/class")
    name=""
    if have lspci; then name=$(lspci -s "$bdf" -mm 2>/dev/null | awk -F'"' '{ print $4 " " $6 }' | head -1); fi
    driver=""; [ -L "$dev/driver" ] && driver=$(basename "$(readlink "$dev/driver")")
    group=""; [ -L "$dev/iommu_group" ] && group=$(basename "$(readlink "$dev/iommu_group")")
    pci_items+=("$(printf '{"bdf":%s,"vendor":%s,"device":%s,"subsys_vendor":%s,"subsys_device":%s,"revision":%s,"class":%s,"name":%s,"driver":%s,"iommu_group":%s}' \
        "$(jstr "$bdf")" "$(jstr "$(hx "$dev/vendor")")" "$(jstr "$(hx "$dev/device")")" \
        "$(jstr "$(hx "$dev/subsystem_vendor")")" "$(jstr "$(hx "$dev/subsystem_device")")" \
        "$(jstr "$(hx "$dev/revision")")" "$(jstr "$class")" "$(jstr "$name")" "$(jstr "$driver")" "$(jnum "$group")")")
done
note pci-sysfs "${#pci_items[@]} functions"

# -------------------------------------------------------------- storage
drive_items=()
for blk in /sys/block/*; do
    b=${blk##*/}
    case "$b" in nvme*n*|sd*|vd*|mmcblk[0-9]) ;; *) continue ;; esac
    path=$(readlink -f "$blk")
    bus=scsi
    case "$b" in nvme*) bus=nvme ;; vd*) bus=virtio ;; mmcblk*) bus=mmc ;; esac
    case "$path" in */usb*) bus=usb ;; esac
    model=$(rd "$blk/device/model"); [ -n "$model" ] || model=$(rd "$blk/device/name")
    fwrev=$(rd "$blk/device/firmware_rev"); [ -n "$fwrev" ] || fwrev=$(rd "$blk/device/rev")
    sectors=$(rd "$blk/size")
    size=""; [[ "$sectors" =~ ^[0-9]+$ ]] && size=$((sectors * 512))
    media=SSD; [ "$(rd "$blk/queue/rotational")" = 1 ] && media=HDD
    drive_items+=("$(printf '{"model":%s,"bus":"%s","media":"%s","size_bytes":%s,"firmware":%s}' \
        "$(jstr "$model")" "$bus" "$media" "$(jnum "$size")" "$(jstr "$fwrev")")")
done
note storage "${#drive_items[@]} drives"

# -------------------------------------------------------------- network
nic_items=()
for ifc in /sys/class/net/*; do
    [ -e "$ifc/device/vendor" ] || continue
    d=$(readlink -f "$ifc/device")
    vendor=$(hx "$ifc/device/vendor"); device=$(hx "$ifc/device/device")
    name=""
    if have lspci; then name=$(lspci -s "${d##*/}" -mm 2>/dev/null | awk -F'"' '{ print $4 " " $6 }' | head -1); fi
    driver=""; [ -L "$ifc/device/driver" ] && driver=$(basename "$(readlink "$ifc/device/driver")")
    nic_items+=("$(printf '{"name":%s,"manufacturer":null,"bus":"pci","vendor":%s,"device":%s,"driver":%s}' \
        "$(jstr "$name")" "$(jstr "$vendor")" "$(jstr "$device")" "$(jstr "$driver")")")
done

# ----------------------------------------------------------------- ACPI
T=/sys/firmware/acpi/tables
acpi_items=()
skipped_acpi=0
if [ -d "$T" ] && [ -r "$T/FACP" ]; then
    for f in "$T"/*; do
        [ -f "$f" ] || continue
        s=${f##*/}
        case " $ALLOWED_ACPI " in *" $s "*) ;; *) skipped_acpi=$((skipped_acpi + 1)) ;; esac
    done
    for s in $ALLOWED_ACPI; do
        f="$T/$s"
        [ -r "$f" ] || continue
        cp "$f" "$OUT/acpi/$s.bin"
        bin="$OUT/acpi/$s.bin"
        instances=$(find "$T" -maxdepth 1 -type f \( -name "$s" -o -name "$s[0-9]*" \) | wc -l)
        u32() { od -An -t u4 -j "$1" -N4 "$bin" | tr -d ' '; }
        u8() { od -An -t u1 -j "$1" -N1 "$bin" | tr -d ' '; }
        chars() { tail -c +"$(($1 + 1))" "$bin" | head -c "$2" | tr -cd '[:print:]'; }
        sum=$(od -An -v -t u1 "$bin" | awk '{ for (i = 1; i <= NF; i++) s += $i } END { print s % 256 }')
        ok=false; [ "$sum" = 0 ] && ok=true
        acpi_items+=("$(printf '{"signature":"%s","instances":%s,"length":%s,"revision":%s,"checksum_ok":%s,"oem_id":%s,"oem_table_id":%s,"oem_revision":%s,"creator_id":%s,"creator_revision":%s,"sha256":"%s","data_base64":"%s"}' \
            "$s" "$instances" "$(u32 4)" "$(u8 8)" "$ok" "$(jstr "$(chars 10 6)")" "$(jstr "$(chars 16 8)")" \
            "$(u32 24)" "$(jstr "$(chars 28 4)")" "$(u32 32)" \
            "$(sha256sum "$bin" | cut -d' ' -f1)" "$(base64 -w0 "$bin")")")
    done
    (cd "$OUT/acpi" && if ls ./*.bin >/dev/null 2>&1; then sha256sum ./*.bin >SHA256SUMS; fi)
    note acpi "${#acpi_items[@]} allowlisted tables, $skipped_acpi other tables not read"
else
    skip acpi "$T not readable (needs root) or absent"
fi

# ---------------------------------------------------------------- IOMMU
{
    echo "# IOMMU units (/sys/class/iommu)"
    if [ -d /sys/class/iommu ]; then ls -1 /sys/class/iommu; fi
    echo "# groups (/sys/kernel/iommu_groups)"
    if [ -d /sys/kernel/iommu_groups ]; then
        for g in /sys/kernel/iommu_groups/*; do
            [ -d "$g/devices" ] || continue
            echo "group ${g##*/}: $(ls -1 "$g/devices" | tr '\n' ' ')"
        done
    fi
} >"$OUT/iommu-groups.txt"
units=()
if [ -d /sys/class/iommu ]; then
    for u in /sys/class/iommu/*; do [ -e "$u" ] && units+=("$(jstr "${u##*/}")"); done
fi
ngroups=0
[ -d /sys/kernel/iommu_groups ] && ngroups=$(find /sys/kernel/iommu_groups -mindepth 1 -maxdepth 1 -type d | wc -l)
fw_tables=()
for a in "${acpi_items[@]+"${acpi_items[@]}"}"; do
    case "$a" in *'"signature":"IVRS"'*) fw_tables+=('"IVRS"') ;; *'"signature":"DMAR"'*) fw_tables+=('"DMAR"') ;; esac
done
iommu_json=$(printf '{"firmware_tables":[%s],"units":[%s],"groups":%s}' \
    "$(join_by_comma "${fw_tables[@]+"${fw_tables[@]}"}")" "$(join_by_comma "${units[@]+"${units[@]}"}")" "$ngroups")
note iommu "${#units[@]} units, $ngroups groups"

if have dmesg && dmesg >/dev/null 2>&1; then
    dmesg | grep -iE 'iommu|AMD-Vi|DMAR|x2apic|smp' | scrub_text >"$OUT/dmesg-iommu.txt" || true
    note dmesg ok
else
    skip dmesg "dmesg unavailable or restricted (kernel.dmesg_restrict)"
fi

# ------------------------------------------------------------ inventory
os_name=""
[ -r /etc/os-release ] && os_name=$(. /etc/os-release && printf '%s' "${PRETTY_NAME:-}")
limits_json=()
for l in "${LIMITS[@]+"${LIMITS[@]}"}"; do limits_json+=("$(jstr "$l")"); done
{
    printf '{\n"schema_version":1,\n"kind":"nanox-hw-inventory",\n"collector":"collect-linux.sh",\n"collector_version":%s,\n' "$COLLECTOR_VERSION"
    printf '"collected_utc":"%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '"host_os":{"family":"linux","version":%s,"build":%s},\n' "$(jstr "$(uname -r)")" "$(jstr "$os_name")"
    printf '"privileged":%s,\n' "$(if [ $IS_ROOT -eq 1 ]; then echo true; else echo false; fi)"
    printf '"system":%s,\n"bios":%s,\n"firmware":%s,\n"cpu":%s,\n"memory":%s,\n' \
        "$system_json" "$bios_json" "$firmware_json" "$cpu_json" "$memory_json"
    printf '"pci":[\n%s\n],\n' "$(IFS=$'\n'; printf '%s' "${pci_items[*]+"${pci_items[*]}"}" | paste -sd, -)"
    printf '"storage_drives":[%s],\n' "$(join_by_comma "${drive_items[@]+"${drive_items[@]}"}")"
    printf '"network_adapters":[%s],\n' "$(join_by_comma "${nic_items[@]+"${nic_items[@]}"}")"
    printf '"acpi_tables":[\n%s\n],\n' "$(IFS=$'\n'; printf '%s' "${acpi_items[*]+"${acpi_items[*]}"}" | paste -sd, -)"
    printf '"acpi_skipped_count":%s,\n"iommu":%s,\n' "$skipped_acpi" "$iommu_json"
    printf '"limitations":[%s]\n}\n' "$(join_by_comma "${limits_json[@]+"${limits_json[@]}"}")"
} >"$OUT/inventory.json"

# ------------------------------------------------------ leak self-check
# Real identifying values, read into memory only, must not remain anywhere.
secrets=()
add_secret() {
    local v
    v=$(printf '%s' "$1" | tr -d '\000\n' | sed -e 's/^ *//' -e 's/ *$//')
    [ ${#v} -ge 4 ] || return 0
    [[ "$v" =~ ^(0+|[Ff]+|None|Default\ string|To\ be\ filled\ by\ O\.E\.M\.|System\ Serial\ Number|Not\ Specified|Not\ Applicable)$ ]] && return 0
    secrets+=("$v")
}
add_secret "$(uname -n)"
add_secret "$(id -un)"
add_secret "${SUDO_USER:-}"
for f in product_serial product_uuid board_serial chassis_serial board_asset_tag chassis_asset_tag; do
    add_secret "$(rd "$D/$f")"
done
for f in /sys/block/*/device/serial /sys/block/*/device/wwid /sys/block/*/device/eui /sys/block/*/device/nguid; do
    [ -r "$f" ] && add_secret "$(rd "$f")"
done
for f in /sys/class/net/*/address /sys/class/net/*/perm_address; do
    [ -r "$f" ] || continue
    mac=$(rd "$f")
    [ "$mac" = "00:00:00:00:00:00" ] && continue
    add_secret "$mac"
    add_secret "${mac//:/-}"
    add_secret "${mac//:/}"
done
for v in "${secrets[@]+"${secrets[@]}"}"; do
    while IFS= read -r -d '' f; do
        case "$f" in *.bin) continue ;; esac
        if grep -qiF -- "$v" "$f"; then
            S="$v" awk '{
                s = tolower(ENVIRON["S"]); line = $0; out = ""
                while ((i = index(tolower(line), s)) > 0) { out = out substr(line, 1, i - 1) "<redacted>"; line = substr(line, i + length(s)) }
                print out line
            }' "$f" >"$f.tmp" && mv "$f.tmp" "$f"
        fi
    done < <(find "$OUT" -type f -print0)
    if grep -rqiF -- "$v" "$OUT"; then
        die "an identifying value (serial/MAC/host or user name) remains in $(grep -rliF -- "$v" "$OUT" | head -1); do not share $OUT"
    fi
done
note leak-check "${#secrets[@]} identifying values checked"
echo "collect-linux: ${#pci_items[@]} PCI functions, ${#drive_items[@]} drives, ${#acpi_items[@]} ACPI tables -> $OUT (see $LOG)"
