<#
.SYNOPSIS
Unprivileged, read-only hardware inventory of a Windows host for NANOX M9.

.DESCRIPTION
Writes one JSON file (schema "nanox-hw-inventory" v1, read by profile.py):
CPU, system/board/BIOS identity, RAM modules, firmware type, Secure Boot
state, PCI functions (IDs, class, BDF), NVMe/disk models, physical network
adapters and an allowlist of ACPI tables read with GetSystemFirmwareTable.

Privacy rules enforced here, not left to the caller:
  * serial numbers, UUIDs, asset tags, MAC addresses, PnP instance paths,
    the computer name and the user name are never selected;
  * only ACPI tables in $AllowedAcpi are read; MSDM (product key), VFCT
    (VBIOS image) and AML are never requested;
  * before writing, the serialized JSON is searched for the real serials,
    MACs, computer and user name (read into memory only) and for MAC/GUID/
    product-key patterns; any hit aborts without writing a file.

Nothing is changed on the machine; no administrator rights are needed.

.EXAMPLE
powershell -NoProfile -ExecutionPolicy Bypass -File collect-windows.ps1 -OutFile inventory.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutFile,
    [switch]$Force
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'

$CollectorVersion = 1
$AllowedAcpi = @('APIC', 'FACP', 'HPET', 'MCFG', 'IVRS', 'DMAR', 'SRAT', 'SLIT')

$OutFile = [IO.Path]::GetFullPath($OutFile)
if ((Test-Path -LiteralPath $OutFile) -and -not $Force) {
    throw "Output file already exists: $OutFile (use -Force to overwrite)"
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NanoxFirmware {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint EnumSystemFirmwareTables(uint provider, byte[] buffer, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint GetSystemFirmwareTable(uint provider, uint id, byte[] buffer, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetFirmwareType(out uint type);
}
'@

$limitations = New-Object System.Collections.Generic.List[string]

function Clean([object]$value) {
    if ($null -eq $value) { return $null }
    $s = ([string]$value) -replace '[\x00-\x1f\x7f]', ' '
    $s = $s.Trim()
    if ($s.Length -eq 0) { return $null }
    return $s
}

function Hex4([string]$s) { return $s.ToLowerInvariant() }

# ---------------------------------------------------------------- system
$cs = Get-CimInstance Win32_ComputerSystem -Property Manufacturer, Model, SystemFamily, SystemSKUNumber, TotalPhysicalMemory, HypervisorPresent
$csp = Get-CimInstance Win32_ComputerSystemProduct -Property Vendor, Name, Version
$bb = Get-CimInstance Win32_BaseBoard -Property Manufacturer, Product, Version
$bios = Get-CimInstance Win32_BIOS -Property Manufacturer, SMBIOSBIOSVersion, ReleaseDate, SMBIOSMajorVersion, SMBIOSMinorVersion, SystemBiosMajorVersion, SystemBiosMinorVersion, EmbeddedControllerMajorVersion, EmbeddedControllerMinorVersion
$os = Get-CimInstance Win32_OperatingSystem -Property Version, BuildNumber

$system = [ordered]@{
    manufacturer       = Clean $cs.Manufacturer
    model              = Clean $cs.Model
    family             = Clean $cs.SystemFamily
    version            = Clean $csp.Version
    sku                = Clean $cs.SystemSKUNumber
    board_manufacturer = Clean $bb.Manufacturer
    board_product      = Clean $bb.Product
    board_version      = Clean $bb.Version
    hypervisor_present = [bool]$cs.HypervisorPresent
}

$biosDate = $null
if ($bios.ReleaseDate) { $biosDate = ([datetime]$bios.ReleaseDate).ToString('yyyy-MM-dd') }
function VersionPair($major, $minor) {
    if ($null -eq $major -or $null -eq $minor -or $major -eq 255) { return $null }
    return "$major.$minor"
}
$biosInfo = [ordered]@{
    vendor         = Clean $bios.Manufacturer
    version        = Clean $bios.SMBIOSBIOSVersion
    date           = $biosDate
    smbios_version = VersionPair $bios.SMBIOSMajorVersion $bios.SMBIOSMinorVersion
    release        = VersionPair $bios.SystemBiosMajorVersion $bios.SystemBiosMinorVersion
    ec_release     = VersionPair $bios.EmbeddedControllerMajorVersion $bios.EmbeddedControllerMinorVersion
}

# -------------------------------------------------------------- firmware
$fwType = 'unknown'
[uint32]$fwRaw = 0
if ([NanoxFirmware]::GetFirmwareType([ref]$fwRaw)) {
    switch ($fwRaw) { 1 { $fwType = 'legacy' } 2 { $fwType = 'uefi' } }
}
$secureBoot = 'unknown'
try {
    $sb = Get-ItemPropertyValue -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State' -Name UEFISecureBootEnabled
    if ($sb -eq 1) { $secureBoot = 'enabled' } elseif ($sb -eq 0) { $secureBoot = 'disabled' }
} catch {
    $limitations.Add('Secure Boot state: registry value not readable; Confirm-SecureBootUEFI needs administrator rights')
}
$dmaProtection = $null
try {
    $dg = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard -Property AvailableSecurityProperties
    # Property 3 = "DMA protection" (Device Guard documentation).
    $dmaProtection = @($dg.AvailableSecurityProperties) -contains 3
} catch {
    $limitations.Add('Device Guard DMA-protection property not readable')
}
$firmware = [ordered]@{
    type                     = $fwType
    secure_boot              = $secureBoot
    dma_protection_available = $dmaProtection
}

# ------------------------------------------------------------------- CPU
$procs = @(Get-CimInstance Win32_Processor -Property Name, Manufacturer, Caption, NumberOfCores, NumberOfLogicalProcessors, MaxClockSpeed, L2CacheSize, L3CacheSize, VirtualizationFirmwareEnabled)
$p0 = $procs[0]
$fam = $null; $mod = $null; $step = $null
if ($p0.Caption -match 'Family (\d+) Model (\d+) Stepping (\d+)') {
    $fam = [int]$Matches[1]; $mod = [int]$Matches[2]; $step = [int]$Matches[3]
}
$cores = 0; $threads = 0
foreach ($p in $procs) { $cores += [int]$p.NumberOfCores; $threads += [int]$p.NumberOfLogicalProcessors }
$cpu = [ordered]@{
    name                  = Clean $p0.Name
    vendor                = Clean $p0.Manufacturer
    family                = $fam
    model                 = $mod
    stepping              = $step
    packages              = $procs.Count
    cores                 = $cores
    threads               = $threads
    max_mhz               = [int]$p0.MaxClockSpeed
    l2_kib                = [int]$p0.L2CacheSize
    l3_kib                = [int]$p0.L3CacheSize
    virtualization_enabled = [bool]$p0.VirtualizationFirmwareEnabled
}

# ---------------------------------------------------------------- memory
$modules = @()
[uint64]$installed = 0
foreach ($m in @(Get-CimInstance Win32_PhysicalMemory -Property Capacity, Speed, ConfiguredClockSpeed, Manufacturer, PartNumber, SMBIOSMemoryType, DeviceLocator, BankLabel)) {
    $installed += [uint64]$m.Capacity
    $modules += [ordered]@{
        locator      = Clean $m.DeviceLocator
        bank         = Clean $m.BankLabel
        size_bytes   = [uint64]$m.Capacity
        speed_mts    = [int]$m.Speed
        configured_mts = [int]$m.ConfiguredClockSpeed
        manufacturer = Clean $m.Manufacturer
        part_number  = Clean $m.PartNumber
        smbios_type  = [int]$m.SMBIOSMemoryType
    }
}
$memory = [ordered]@{
    installed_bytes = $installed
    visible_bytes   = [uint64]$cs.TotalPhysicalMemory
    modules         = $modules
}

# ------------------------------------------------------------------- PCI
# Only hardware IDs are parsed; the instance path is dropped because for
# PCIe functions with a Device Serial Number capability it embeds that
# serial (for NICs usually derived from the MAC).
$hwRe = '^PCI\\VEN_([0-9A-F]{4})&DEV_([0-9A-F]{4})&SUBSYS_([0-9A-F]{4})([0-9A-F]{4})&REV_([0-9A-F]{2})$'
$keys = 'DEVPKEY_Device_BusNumber', 'DEVPKEY_Device_Address', 'DEVPKEY_Device_HardwareIds', 'DEVPKEY_Device_CompatibleIds', 'DEVPKEY_Device_LocationInfo', 'DEVPKEY_Device_Service'
$pci = @()
foreach ($d in @(Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'PCI\*' })) {
    $props = @{}
    foreach ($x in @(Get-PnpDeviceProperty -InputObject $d -KeyName $keys -ErrorAction SilentlyContinue)) {
        # Missing properties come back as objects without a Data member.
        if ($x.PSObject.Properties['Data']) { $props[$x.KeyName] = $x.Data }
    }
    $hw = @($props['DEVPKEY_Device_HardwareIds']) | Where-Object { $_ -match $hwRe } | Select-Object -First 1
    if (-not $hw) { continue }
    $null = $hw -match $hwRe
    $ids = $Matches
    $class = $null
    foreach ($c in @($props['DEVPKEY_Device_CompatibleIds'])) {
        if ($c -match 'CC_([0-9A-F]{6})$') { $class = Hex4 $Matches[1]; break }
    }
    if (-not $class) {
        foreach ($c in @($props['DEVPKEY_Device_CompatibleIds'])) {
            if ($c -match 'CC_([0-9A-F]{4})$') { $class = (Hex4 $Matches[1]) + '00'; break }
        }
    }
    $bus = $props['DEVPKEY_Device_BusNumber']; $addr = $props['DEVPKEY_Device_Address']
    $bdf = $null
    if ($null -ne $bus -and $null -ne $addr) {
        $bdf = '0000:{0:x2}:{1:x2}.{2:x}' -f [int]$bus, ([int]$addr -shr 16), ([int]$addr -band 0xffff)
    } elseif ([string]$props['DEVPKEY_Device_LocationInfo'] -match 'PCI bus (\d+), device (\d+), function (\d+)') {
        $bdf = '0000:{0:x2}:{1:x2}.{2:x}' -f [int]$Matches[1], [int]$Matches[2], [int]$Matches[3]
    }
    $pci += [ordered]@{
        bdf           = $bdf
        vendor        = Hex4 $ids[1]
        device        = Hex4 $ids[2]
        subsys_vendor = Hex4 $ids[4]
        subsys_device = Hex4 $ids[3]
        revision      = Hex4 $ids[5]
        class         = $class
        name          = Clean $d.FriendlyName
        driver        = Clean $props['DEVPKEY_Device_Service']
    }
}
$pci = @($pci | Sort-Object { $_.bdf })
$limitations.Add('PCI: config space, BARs and capabilities are not readable without a kernel driver; functions owned by the hypervisor (e.g. the AMD IOMMU) are not listed')

# --------------------------------------------------------------- storage
$drives = @()
foreach ($pd in @(Get-PhysicalDisk | Sort-Object DeviceId)) {
    $drives += [ordered]@{
        model      = Clean $pd.FriendlyName
        bus        = Clean ([string]$pd.BusType)
        media      = Clean ([string]$pd.MediaType)
        size_bytes = [uint64]$pd.Size
        firmware   = Clean $pd.FirmwareVersion
    }
}

# --------------------------------------------------------------- network
$nics = @()
foreach ($n in @(Get-CimInstance Win32_NetworkAdapter -Filter 'PhysicalAdapter=True' -Property Name, Manufacturer, PNPDeviceID)) {
    $busName = $null; $ven = $null; $dev = $null
    $pnp = [string]$n.PNPDeviceID
    if ($pnp -match '^([A-Z0-9]+)\\') { $busName = $Matches[1].ToLowerInvariant() }
    if ($pnp -match '^PCI\\VEN_([0-9A-F]{4})&DEV_([0-9A-F]{4})') { $ven = Hex4 $Matches[1]; $dev = Hex4 $Matches[2] }
    elseif ($pnp -match '^USB\\VID_([0-9A-F]{4})&PID_([0-9A-F]{4})') { $ven = Hex4 $Matches[1]; $dev = Hex4 $Matches[2] }
    # Software adapters (Bluetooth PAN, virtual switches) are not hardware of the profile.
    if ($busName -ne 'pci' -and $busName -ne 'usb') { continue }
    $nics += [ordered]@{
        name         = Clean $n.Name
        manufacturer = Clean $n.Manufacturer
        bus          = $busName
        vendor       = $ven
        device       = $dev
    }
}

# ------------------------------------------------------------------ ACPI
function SigToUInt32([string]$sig) { return [BitConverter]::ToUInt32([Text.Encoding]::ASCII.GetBytes($sig), 0) }
$provAcpi = [uint32]0x41435049  # 'ACPI'
$size = [NanoxFirmware]::EnumSystemFirmwareTables($provAcpi, $null, 0)
$sigBuf = New-Object byte[] $size
$null = [NanoxFirmware]::EnumSystemFirmwareTables($provAcpi, $sigBuf, $size)
$present = @()
for ($i = 0; $i + 4 -le $size; $i += 4) { $present += [Text.Encoding]::ASCII.GetString($sigBuf, $i, 4) }
$sha = [Security.Cryptography.SHA256]::Create()
$acpi = @()
$skipped = 0
foreach ($sig in $present) {
    if ($AllowedAcpi -notcontains $sig) { $skipped++; continue }
    if (@($acpi | Where-Object { $_.signature -eq $sig }).Count -gt 0) { continue }  # the API returns only the first instance
    $id = SigToUInt32 $sig
    $len = [NanoxFirmware]::GetSystemFirmwareTable($provAcpi, $id, $null, 0)
    if ($len -lt 36) { continue }
    $buf = New-Object byte[] $len
    $null = [NanoxFirmware]::GetSystemFirmwareTable($provAcpi, $id, $buf, $len)
    if ([Text.Encoding]::ASCII.GetString($buf, 0, 4) -ne $sig) { throw "ACPI ${sig}: returned table has another signature" }
    $sum = 0; foreach ($b in $buf) { $sum = ($sum + $b) -band 0xff }
    $acpi += [ordered]@{
        signature    = $sig
        instances    = @($present | Where-Object { $_ -eq $sig }).Count
        length       = [int]$len
        revision     = [int]$buf[8]
        checksum_ok  = ($sum -eq 0)
        oem_id       = Clean ([Text.Encoding]::ASCII.GetString($buf, 10, 6))
        oem_table_id = Clean ([Text.Encoding]::ASCII.GetString($buf, 16, 8))
        oem_revision = [BitConverter]::ToUInt32($buf, 24)
        creator_id   = Clean ([Text.Encoding]::ASCII.GetString($buf, 28, 4))
        creator_revision = [BitConverter]::ToUInt32($buf, 32)
        sha256       = ([BitConverter]::ToString($sha.ComputeHash($buf)) -replace '-', '').ToLowerInvariant()
        data_base64  = [Convert]::ToBase64String($buf)
    }
}
$acpi = @($acpi | Sort-Object { $_.signature })
$limitations.Add('ACPI: Windows does not expose RSDP/XSDT addresses; only the first instance of a signature is returned')
if ($system.hypervisor_present) {
    $limitations.Add('Windows runs as the Hyper-V root partition: the hypervisor owns the IOMMU and may hide or remap devices')
}

$iommu = [ordered]@{
    firmware_tables = @($acpi | Where-Object { $_.signature -eq 'IVRS' -or $_.signature -eq 'DMAR' } | ForEach-Object { $_.signature })
    groups          = $null
}
$limitations.Add('IOMMU groups are not exposed by Windows')

# ---------------------------------------------------------------- output
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$inventory = [ordered]@{
    schema_version     = 1
    kind               = 'nanox-hw-inventory'
    collector          = 'collect-windows.ps1'
    collector_version  = $CollectorVersion
    collected_utc      = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
    host_os            = [ordered]@{ family = 'windows'; version = Clean $os.Version; build = Clean $os.BuildNumber }
    privileged         = $isAdmin
    system             = $system
    bios               = $biosInfo
    firmware           = $firmware
    cpu                = $cpu
    memory             = $memory
    pci                = $pci
    storage_drives     = $drives
    network_adapters   = $nics
    acpi_tables        = $acpi
    acpi_skipped_count = $skipped
    iommu              = $iommu
    limitations        = @($limitations)
}
$json = ($inventory | ConvertTo-Json -Depth 8) -replace "`r`n", "`n"

# ------------------------------------------------------- leak self-check
# Sensitive values are read into memory only to prove they are absent.
$secrets = New-Object System.Collections.Generic.List[string]
function AddSecret([object]$v) {
    $s = Clean $v
    if ($null -eq $s -or $s.Length -lt 4) { return }
    if ($s -match '^(0+|F+|None|Default string|To be filled by O\.E\.M\.|System Serial Number|Not Specified|Not Applicable)$') { return }
    $secrets.Add($s)
}
AddSecret $env:COMPUTERNAME
AddSecret $env:USERNAME
AddSecret ((Get-CimInstance Win32_BIOS -Property SerialNumber).SerialNumber)
AddSecret ((Get-CimInstance Win32_BaseBoard -Property SerialNumber).SerialNumber)
$prod = Get-CimInstance Win32_ComputerSystemProduct -Property IdentifyingNumber, UUID
AddSecret $prod.IdentifyingNumber
AddSecret $prod.UUID
foreach ($e in @(Get-CimInstance Win32_SystemEnclosure -Property SerialNumber, SMBIOSAssetTag)) { AddSecret $e.SerialNumber; AddSecret $e.SMBIOSAssetTag }
foreach ($x in @(Get-CimInstance Win32_DiskDrive -Property SerialNumber)) { AddSecret $x.SerialNumber }
foreach ($x in @(Get-PhysicalDisk)) { AddSecret $x.SerialNumber }
foreach ($x in @(Get-CimInstance Win32_PhysicalMemory -Property SerialNumber)) { AddSecret $x.SerialNumber }
foreach ($x in @(Get-CimInstance Win32_NetworkAdapter -Property MACAddress)) {
    $mac = Clean $x.MACAddress
    if ($mac) { AddSecret $mac; AddSecret ($mac -replace ':', '-'); AddSecret ($mac -replace ':', '') }
}
foreach ($s in $secrets) {
    if ($json.IndexOf($s, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw 'Refusing to write: output contains a serial number, UUID, MAC address, computer or user name'
    }
}
$patterns = @(
    '(?i)\b([0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b',                                  # MAC
    '(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b',      # UUID/GUID
    '\b[A-Z0-9]{5}(-[A-Z0-9]{5}){4}\b'                                           # product key
)
foreach ($re in $patterns) {
    if ($json -match $re) { throw "Refusing to write: output matches a sensitive pattern ($re)" }
}

$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path -LiteralPath $dir)) { throw "Output directory does not exist: $dir" }
[IO.File]::WriteAllText($OutFile, $json + "`n", (New-Object Text.UTF8Encoding($false)))
Write-Host ("collect-windows: {0} PCI functions, {1} drives, {2} ACPI tables ({3} other tables not read) -> {4}" -f $pci.Count, $drives.Count, $acpi.Count, $skipped, $OutFile)
