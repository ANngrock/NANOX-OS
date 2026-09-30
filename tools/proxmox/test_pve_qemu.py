import unittest

import pve_qemu as pq

KW = dict(disk="/d/disk.img", code_fd="/d/code.fd", vars_fd="/d/vars.fd", serial_log="/d/serial.log")


def pairs(argv):
    """[(flag, value)] for the two-token options."""
    return [(argv[i], argv[i + 1]) for i in range(len(argv) - 1) if argv[i].startswith("-")]


class ArgvShape(unittest.TestCase):
    def test_uefi_q35_like_qm_showcmd(self):
        a = pq.pve_argv(**KW)
        p = pairs(a)
        self.assertIn(("-machine", "q35"), p)
        # OVMF as two pflash units: code read-only, variables writable.
        flash = [v for f, v in p if f == "-drive" and v.startswith("if=pflash")]
        self.assertEqual(len(flash), 2)
        self.assertIn("unit=0", flash[0])
        self.assertIn("readonly=on", flash[0])
        self.assertIn("unit=1", flash[1])
        self.assertNotIn("readonly", flash[1])
        self.assertIn("-nodefaults", a)

    def test_boot_disk_is_virtio_scsi_with_a_boot_index(self):
        a = pq.pve_argv(**KW)
        p = pairs(a)
        self.assertIn(("-device", "virtio-scsi-pci,id=virtioscsi0,bus=pcie.0"), p)
        scsi = [v for f, v in p if f == "-device" and v.startswith("scsi-hd")]
        self.assertEqual(len(scsi), 1)
        self.assertIn("bootindex=100", scsi[0])
        self.assertIn("drive=drive-scsi0", scsi[0])
        drive = [v for f, v in p if f == "-drive" and "id=drive-scsi0" in v]
        self.assertEqual(len(drive), 1)
        self.assertIn("if=none", drive[0])

    def test_full_profile_has_the_guest_agent_channel_under_its_reserved_name(self):
        a = pq.pve_argv(**KW, profile="full", qga_sock="/d/qga.sock")
        j = " ".join(a)
        self.assertIn("name=org.qemu.guest_agent.0", j)
        self.assertIn("virtio-serial", j)
        self.assertIn("socket,path=/d/qga.sock,server=on,wait=off,id=qga0", j)
        for dev in ("virtio-balloon-pci", "virtio-rng-pci", "virtio-net-pci", "i6300esb", "VGA"):
            self.assertIn(dev, j)
        self.assertIn("-watchdog-action reset", j)

    def test_min_profile_has_nothing_extra(self):
        j = " ".join(pq.pve_argv(**KW, profile="min", qga_sock="/d/qga.sock"))
        for dev in ("virtio-balloon", "virtio-net", "i6300esb", "org.qemu.guest_agent.0"):
            self.assertNotIn(dev, j)

    def test_no_shutdown_is_opt_in(self):
        self.assertNotIn("-no-shutdown", pq.pve_argv(**KW))
        self.assertIn("-no-shutdown", pq.pve_argv(**KW, no_shutdown=True))

    def test_debug_exit_and_monitor_options(self):
        a = pq.pve_argv(**KW, qmp_sock="/d/qmp.sock")
        self.assertIn("isa-debug-exit,iobase=0xf4,iosize=4", a)
        self.assertIn("unix:/d/qmp.sock,server=on,wait=off", a)
        b = pq.pve_argv(**KW, debug_exit=False)
        self.assertNotIn("isa-debug-exit,iobase=0xf4,iosize=4", b)
        self.assertNotIn("-qmp", b)

    def test_cpus_and_memory(self):
        a = pq.pve_argv(**KW, cores=4, ram_mib=2048)
        p = dict(pairs(a))
        self.assertEqual(p["-smp"], "4,sockets=1,cores=4,maxcpus=4")
        self.assertEqual(p["-m"], "2048")

    def test_unknown_profile_is_refused(self):
        with self.assertRaises(ValueError):
            pq.pve_argv(**KW, profile="huge")

    def test_no_backend_the_pinned_qemu_lacks(self):
        # The pinned QEMU has neither slirp ("user") nor TAP.
        j = " ".join(pq.pve_argv(**KW, profile="full"))
        self.assertNotIn("user,", j)
        self.assertNotIn("tap", j)


if __name__ == "__main__":
    unittest.main()
