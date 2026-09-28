//! End to end: a model USB keyboard behind the model xHCI controller, the
//! xHCI driver enumerating and configuring it, interrupt IN reports decoded
//! by `hid-keyboard` into text, and the Caps Lock LED sent back with
//! SET_REPORT.

mod common;

use common::*;
use hid_keyboard::{Action, BootReport, Config as KbConfig, Key, Keyboard, Layout, Modifiers};
use hw_xhci::descriptor::{find_boot_keyboard, SetupPacket};
use hw_xhci::{Controller, Notification};

/// (usage, shift) producing `c` in `layout`, via the decoder's public API.
fn find_key(layout: Layout, c: char) -> (u8, bool) {
    for shift in [false, true] {
        for usage in 0x04..=0x38u8 {
            let mut kb = Keyboard::new(KbConfig {
                layouts: [layout, layout],
                ..KbConfig::default()
            });
            let mut hit = false;
            let rep = report(if shift { Modifiers::LEFT_SHIFT } else { 0 }, Some(usage));
            kb.feed(&BootReport::parse(&rep).unwrap(), 0, &mut |e| {
                hit |= e.key == Key::Char(c)
            });
            if hit {
                return (usage, shift);
            }
        }
    }
    panic!("{c:?} not on {layout:?}");
}

fn report(mods: u8, key: Option<u8>) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0] = mods;
    if let Some(k) = key {
        b[2] = k;
    }
    b
}

/// A configured keyboard on port 1 and the decoder fed from it.
struct Session {
    hw: Hw,
    c: Controller,
    kb: Keyboard,
    slot: u8,
    dci: u8,
    interface: u8,
    now: u64,
    text: String,
}

impl Session {
    fn new() -> Self {
        let (mut hw, mut c) = running(ModelConfig::default(), config());
        hw.connect(1, UsbDevice::keyboard());
        let dev = c.attach(&mut hw, 1).expect("attach");
        let mut buf = [0u8; 256];
        let n = c
            .read_configuration(&mut hw, dev.slot, 0, &mut buf)
            .unwrap();
        let boot = find_boot_keyboard(&buf[..n])
            .unwrap()
            .expect("boot keyboard");
        let dci = c
            .configure_interrupt_in(&mut hw, dev.slot, boot.config_value, &boot.endpoint, None)
            .unwrap();
        c.control_out(
            &mut hw,
            dev.slot,
            SetupPacket::hid_set_protocol(boot.interface, 0),
            &[],
        )
        .expect("SET_PROTOCOL(boot)");
        Self {
            hw,
            c,
            kb: Keyboard::new(KbConfig::default()),
            slot: dev.slot,
            dci,
            interface: boot.interface,
            now: 0,
            text: String::new(),
        }
    }

    /// One interrupt IN transfer carrying `rep` from the device to the
    /// decoder.
    fn send(&mut self, rep: [u8; 8]) {
        self.now += 20;
        let id = self
            .c
            .submit_interrupt_in(&mut self.hw, self.slot, self.dci, USER_BASE, 8)
            .expect("submit");
        self.hw.push_report(1, &rep);
        let mut got = None;
        for _ in 0..10_000 {
            if let Some(Notification::Transfer(t)) = self.c.poll(&mut self.hw).expect("poll") {
                assert_eq!(t.id, id);
                got = Some(t.result);
                break;
            }
        }
        assert_eq!(got, Some(Ok(8)), "report not delivered");
        let mut b = [0u8; 8];
        self.hw.mem_read(USER_BASE, &mut b);
        let text = &mut self.text;
        self.kb
            .feed(&BootReport::parse(&b).unwrap(), self.now, &mut |e| {
                if e.action == Action::Press {
                    if let Key::Char(ch) = e.key {
                        text.push(ch);
                    }
                }
            });
    }

    fn type_str(&mut self, s: &str) {
        for ch in s.chars() {
            let (u, shift) = find_key(self.kb.layout(), ch);
            let m = if shift { Modifiers::LEFT_SHIFT } else { 0 };
            self.send(report(m, Some(u)));
            self.send(report(0, None));
        }
    }
}

#[test]
fn typing_through_the_controller() {
    let mut s = Session::new();
    s.type_str("Hello, NANOX! ");
    // Alt+Shift to Russian.
    s.send(report(Modifiers::LEFT_ALT, None));
    s.send(report(Modifiers::LEFT_ALT | Modifiers::LEFT_SHIFT, None));
    s.send(report(0, None));
    assert_eq!(s.kb.layout(), Layout::Russian);
    s.type_str("Привет, мир!");
    assert_eq!(s.text, "Hello, NANOX! Привет, мир!");

    // Caps Lock: the decoder asks for an LED update, the driver sends it.
    s.send(report(0, Some(0x39)));
    s.send(report(0, None));
    let leds = s.kb.take_led_update().expect("LED change");
    let set_report = SetupPacket {
        request_type: 0x21,
        request: 0x09,
        value: 0x0200,
        index: u16::from(s.interface),
        length: 1,
    };
    s.c.control_out(&mut s.hw, s.slot, set_report, &[leds])
        .expect("SET_REPORT");
    let last = *s.hw.device(1).unwrap().setups.last().unwrap();
    assert_eq!(last[..2], [0x21, 0x09]);
    assert_eq!(last[2..4], [0x00, 0x02], "output report");
    s.hw.assert_clean();
}
