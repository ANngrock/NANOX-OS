kernel: qemu exit 0; guest report complete: True; 196156 trace lines

### Device regions touched (QEMU memory_region_ops: I/O ports and MMIO)

| region | reads | writes | distinct addresses | lowest | highest |
|---|---|---|---|---|---|
| vga-lowmem | 10094 | 84138 | 36864 | 0xa0000 | 0xbffff |
| pcspk | 44042 | 4 | 1 | 0x61 | 0x61 |
| pci-conf-idx | 3 | 9119 | 2 | 0xcf8 | 0xcfb |
| pci-conf-data | 8990 | 125 | 4 | 0xcfc | 0xcff |
| serial | 2010 | 4400 | 7 | 0x3f8 | 0x3fe |
| apic-msi | 80 | 5773 | 34 | 0xfee00000 | 0xfee003e0 |
| vga | 410 | 2958 | 11 | 0x3c0 | 0x3da |
| io | 15 | 3290 | 7 | 0xf1 | 0x402 |
| hpet | 738 | 698 | 11 | 0xfed00000 | 0xfed00148 |
| ahci | 975 | 91 | 54 | 0xfebd5000 | 0xfebd53a8 |
| acpi-tmr | 1042 | 0 | 1 | 0x608 | 0x608 |
| ioapic | 311 | 699 | 2 | 0xfec00000 | 0xfec00010 |
| rtc-index | 382 | 116 | 1 | 0x70 | 0x70 |
| port92 | 384 | 2 | 1 | 0x92 | 0x92 |
| pcie-mmcfg-mmio | 178 | 138 | 89 | 0xb0000002 | 0xb00fb040 |
| edid | 256 | 0 | 256 | 0xfebd4000 | 0xfebd40ff |
| pit | 220 | 15 | 3 | 0x40 | 0x43 |
| ioport80 | 0 | 200 | 1 | 0x80 | 0x80 |
| i8042-cmd | 128 | 40 | 1 | 0x64 | 0x64 |
| rtc | 111 | 5 | 1 | 0x71 | 0x71 |
| pic | 22 | 88 | 4 | 0x20 | 0xa1 |
| i8042-data | 31 | 43 | 1 | 0x60 | 0x60 |
| acpi-gpe0 | 20 | 52 | 16 | 0x620 | 0x62f |
| fwcfg.dma | 0 | 57 | 1 | 0x518 | 0x518 |
| apm-io | 12 | 14 | 2 | 0xb2 | 0xb3 |
| acpi-evt | 11 | 13 | 2 | 0x600 | 0x602 |
| vbe | 5 | 17 | 2 | 0x1ce | 0x1cf |
| fwcfg | 8 | 3 | 2 | 0x510 | 0x511 |
| acpi-cpu-hotplug | 2 | 6 | 5 | 0xcd8 | 0xcdc |
| acpi-cnt | 4 | 2 | 1 | 0x604 | 0x604 |
| dma-cont | 0 | 4 | 4 | 0xd | 0xda |
| parallel | 2 | 2 | 2 | 0x378 | 0x37a |
| elcr | 0 | 2 | 2 | 0x4d0 | 0x4d1 |
| acpi-smi | 1 | 1 | 1 | 0x630 | 0x630 |
| kvmvapic | 0 | 1 | 1 | 0x7e | 0x7e |
| ioportF0 | 0 | 1 | 1 | 0xf0 | 0xf0 |
| dma-page | 1 | 0 | 1 | 0x87 | 0x87 |

### PCI configuration space accessed

| device | address | reads | writes | distinct offsets |
|---|---|---|---|---|
| mch | 00:00.0 | 111 | 42 | 27 |
| VGA | 00:01.0 | 108 | 40 | 22 |
| e1000e | 00:02.0 | 207 | 47 | 43 |
| ICH9-LPC | 00:1f.0 | 165 | 55 | 39 |
| ich9-ahci | 00:1f.2 | 126 | 38 | 24 |
| ICH9-SMB | 00:1f.3 | 91 | 39 | 22 |
| (absent slots probed) | 8193 addresses | 8359 | 0 | - |

### Interrupt controller and timer accesses

| event | count |
|---|---|
| apic_register_write | 1595 |
| hpet_ram_read | 738 |
| ioapic_mem_write | 699 |
| hpet_ram_write | 698 |
| ioapic_mem_read | 311 |
| apic_register_read | 80 |

### What the guest reports about its machine

```
--- ioports
0000-0cf7 : PCI Bus 0000:00
  0000-001f : dma1
  0020-0021 : pic1
  0040-0043 : timer0
  0050-0053 : timer1
  0060-0060 : keyboard
  0064-0064 : keyboard
  0070-0077 : rtc0
  0080-008f : dma page reg
  00a0-00a1 : pic2
  00c0-00df : dma2
  00f0-00ff : fpu
  03c0-03df : vga+
  03f8-03ff : serial
  0510-051b : QEMU0002:00
  0600-067f : 0000:00:1f.0
    0600-0603 : ACPI PM1a_EVT_BLK
    0604-0605 : ACPI PM1a_CNT_BLK
    0608-060b : ACPI PM_TMR
    0620-062f : ACPI GPE0_BLK
  0700-073f : 0000:00:1f.3
0cf8-0cff : PCI conf1
0d00-ffff : PCI Bus 0000:00
  c040-c05f : 0000:00:02.0
  c060-c07f : 0000:00:1f.2
--- iomem
00000000-00000fff : Reserved
00001000-0009fbff : System RAM
0009fc00-0009ffff : Reserved
000a0000-000bffff : PCI Bus 0000:00
000c0000-000c99ff : Video ROM
000ca000-000cadff : Adapter ROM
000cb000-000cb5ff : Adapter ROM
000f0000-000fffff : Reserved
  000f0000-000fffff : System ROM
00100000-1ffdefff : System RAM
  01000000-021f6167 : Kernel code
  02200000-02e91fff : Kernel rodata
  03000000-0328ef7f : Kernel data
  036a8000-039fffff : Kernel bss
1ffdf000-1fffffff : Reserved
20000000-afffffff : PCI Bus 0000:00
b0000000-bfffffff : PCI ECAM 0000 [bus 00-ff]
  b0000000-bfffffff : Reserved
    b0000000-bfffffff : pnp 00:05
c0000000-febfffff : PCI Bus 0000:00
  fd000000-fdffffff : 0000:00:01.0
  feb40000-feb7ffff : 0000:00:02.0
  feb80000-feb9ffff : 0000:00:02.0
  feba0000-febbffff : 0000:00:02.0
  febd0000-febd3fff : 0000:00:02.0
  febd4000-febd4fff : 0000:00:01.0
  febd5000-febd5fff : 0000:00:1f.2
fec00000-fec003ff : IOAPIC 0
fed00000-fed003ff : HPET 0
  fed00000-fed003ff : PNP0103:00
fed1c000-fed1ffff : Reserved
fffc0000-ffffffff : Reserved
100000000-8ffffffff : PCI Bus 0000:00
fd00000000-ffffffffff : Reserved
--- interrupts
           CPU0       
  0:        145  IO-APIC   2-edge      timer
  1:          9  IO-APIC   1-edge      i8042
  4:        264  IO-APIC   4-edge      ttyS0
  8:          1  IO-APIC   8-edge      rtc0
  9:          0  IO-APIC   9-fasteoi   acpi
 12:          5  IO-APIC  12-edge      i8042
NMI:          0   Non-maskable interrupts
LOC:        446   Local timer interrupts
SPU:          0   Spurious interrupts
PMI:          0   Performance monitoring interrupts
IWI:          0   IRQ work interrupts
RTR:          0   APIC ICR read retries
RES:          0   Rescheduling interrupts
CAL:          0   Function call interrupts
TLB:          0   TLB shootdowns
TRM:          0   Thermal event interrupts
THR:          0   Threshold APIC interrupts
DFR:          0   Deferred Error APIC interrupts
MCE:          0   Machine check exceptions
MCP:          0   Machine check polls
ERR:          0
MIS:          0
PIN:          0   Posted-interrupt notification event
NPI:          0   Nested posted-interrupt event
PIW:          0   Posted-interrupt wakeup event
--- clocksource
tsc-early
--- devices
Character devices:
  1 mem
  4 /dev/vc/0
  4 tty
  4 ttyS
  5 /dev/tty
  5 /dev/console
  5 /dev/ptmx
  7 vcs
 10 misc
 13 input
 21 sg
128 ptm
136 pts
241 intel_mid_scu
242 nvme-generic
243 nvme
244 bsg
245 watchdog
246 ptp
247 pps
248 rtc
249 dax
250 dimmctl
251 ndctl
252 tpm
253 pwm
254 gpiochip

Block devices:
  1 ramdisk
  7 loop
  8 sd
 11 sr
 65 sd
 66 sd
 67 sd
 68 sd
 69 sd
 70 sd
 71 sd
128 sd
129 sd
130 sd
131 sd
132 sd
133 sd
134 sd
135 sd
254 device-mapper
259 blkext
--- pci
0000:00:00.0 vendor 0x8086
 device 0x29c0
0000:00:01.0 vendor 0x1234
 device 0x1111
0000:00:02.0 vendor 0x8086
 device 0x10d3
```
