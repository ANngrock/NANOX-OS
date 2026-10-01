import unittest

import linux_surface as L


def parse_newc(img):
    """Reads back a newc archive: returns [(name, mode, data)]."""
    out, at = [], 0
    while True:
        assert img[at:at + 6] == b"070701", "bad magic"
        f = [int(img[at + 6 + 8 * i:at + 14 + 8 * i], 16) for i in range(13)]
        mode, size, namesize = f[1], f[6], f[11]
        name_at = at + 110
        name = img[name_at:name_at + namesize - 1].decode()
        data_at = (name_at + namesize + 3) & ~3
        data = img[data_at:data_at + size]
        at = (data_at + size + 3) & ~3
        if name == "TRAILER!!!":
            return out
        out.append((name, mode, data))


class Cpio(unittest.TestCase):
    def test_round_trip_with_awkward_sizes(self):
        files = [("init", 0o100755, b"x" * 5), ("a", 0o100644, b""), ("bin/b", 0o100755, b"yz" * 33)]
        self.assertEqual(parse_newc(L.cpio_newc(files)), files)

    def test_entries_are_four_byte_aligned(self):
        img = L.cpio_newc([("n", 0o100644, b"abc")])
        self.assertEqual(len(img) % 4, 0)


class Summary(unittest.TestCase):
    LINES = [
        "memory_region_ops_read cpu 0 mr 0x55 addr 0x71 value 0x0 size 1 name 'rtc'",
        "memory_region_ops_write cpu 0 mr 0x55 addr 0x70 value 0x8f size 1 name 'rtc-index'",
        "memory_region_ops_write cpu 0 mr 0x55 addr 0x70 value 0x8e size 1 name 'rtc-index'",
        "pci_cfg_read mch 00:00.0 @0x0 -> 0x8086",
        "pci_cfg_read empty 00:05.0 @0x0 -> 0xffffffff",
        "apic_register_write register 0x0f = 0x1ff",
        "apic_register_write register 0x35 = 0x8700",
    ]

    def test_regions_pci_and_controllers_are_counted(self):
        text = L.summarize(self.LINES, "NANOX_GUEST_REPORT_BEGIN\nhello\nNANOX_GUEST_REPORT_END")
        self.assertIn("| rtc-index | 0 | 2 | 1 | 0x70 | 0x70 |", text)
        self.assertIn("| rtc | 1 | 0 | 1 | 0x71 | 0x71 |", text)
        self.assertIn("| mch | 00:00.0 | 1 | 0 | 1 |", text)
        self.assertIn("(absent slots probed) | 1 addresses", text)
        self.assertIn("| apic_register_write | 2 |", text)
        self.assertIn("hello", text)

    def test_no_report_means_no_guest_section(self):
        self.assertNotIn("What the guest reports", L.summarize(self.LINES, "boot failed"))


if __name__ == "__main__":
    unittest.main()
