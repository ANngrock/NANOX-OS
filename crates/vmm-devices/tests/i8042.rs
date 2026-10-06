//! The PS/2 controller and keyboard as the Linux i8042 and atkbd drivers meet them.

use vmm_devices::i8042::*;

fn cmd(k: &mut I8042, c: u8) {
    assert!(k.write(PORT_STATUS, c));
}

fn data(k: &mut I8042, v: u8) {
    assert!(k.write(PORT_DATA, v));
}

fn rd(k: &mut I8042) -> u8 {
    k.read(PORT_DATA).unwrap()
}

fn status(k: &mut I8042) -> u8 {
    k.read(PORT_STATUS).unwrap()
}

fn drain(k: &mut I8042) -> Vec<u8> {
    let mut v = Vec::new();
    while status(k) & 1 != 0 {
        v.push(rd(k));
    }
    v
}

#[test]
fn only_its_two_ports() {
    let mut k = I8042::new();
    for p in [0x61, 0x62, 0x63, 0x65, 0x92] {
        assert!(
            !I8042::owns(p) && k.read(p).is_none() && !k.write(p, 0),
            "{p:#x}"
        );
    }
}

#[test]
fn reset_state_and_status_bits() {
    let mut k = I8042::new();
    assert_eq!(
        status(&mut k),
        0x14,
        "not locked, system flag, nothing to read"
    );
    assert_eq!(k.command_byte(), 0x45);
    assert!(!k.irq1());
    cmd(&mut k, 0x20);
    assert_eq!(
        status(&mut k),
        0x14 | 1 | 8,
        "output full; the last write was a command"
    );
    assert_eq!(rd(&mut k), 0x45);
    assert_eq!(status(&mut k), 0x14 | 8);
    data(&mut k, 0xEE);
    assert_eq!(status(&mut k) & 8, 0, "the last write was data");
}

#[test]
fn self_test_and_interface_test() {
    let mut k = I8042::new();
    cmd(&mut k, 0xAA);
    assert_eq!(drain(&mut k), [0x55]);
    cmd(&mut k, 0xAB);
    assert_eq!(drain(&mut k), [0x00]);
    cmd(&mut k, 0xA9);
    assert_eq!(drain(&mut k), [0x00]);
}

#[test]
fn the_self_test_restores_the_default_command_byte_and_clears_the_buffer() {
    let mut k = I8042::new();
    cmd(&mut k, 0x60);
    data(&mut k, 0x00);
    assert_eq!(k.command_byte(), 0);
    cmd(&mut k, 0x60);
    data(&mut k, 0x45);
    k.push_scancode(0x1C);
    cmd(&mut k, 0x60);
    data(&mut k, 0x00);
    cmd(&mut k, 0xAA);
    assert_eq!(drain(&mut k), [0x55], "the pending scancode is gone");
    assert_eq!(k.command_byte(), 0x45);
}

#[test]
fn the_command_byte_is_read_and_written() {
    let mut k = I8042::new();
    cmd(&mut k, 0x60);
    data(&mut k, 0x30);
    cmd(&mut k, 0x20);
    assert_eq!(drain(&mut k), [0x30]);
    cmd(&mut k, 0x21);
    assert_eq!(drain(&mut k), [0], "other command-byte numbers read zero");
    cmd(&mut k, 0x61);
    data(&mut k, 0xFF);
    assert_eq!(k.command_byte(), 0x30, "only 0x60 writes the command byte");
}

#[test]
fn enabling_and_disabling_the_keyboard_port() {
    let mut k = I8042::new();
    cmd(&mut k, 0xAD);
    assert_eq!(k.command_byte() & 0x10, 0x10);
    k.push_scancode(0x1C);
    assert_eq!(status(&mut k) & 1, 0, "a disabled port delivers nothing");
    cmd(&mut k, 0xAE);
    assert_eq!(k.command_byte() & 0x10, 0);
    k.push_scancode(0x1C);
    assert_eq!(status(&mut k) & 1, 1);
}

#[test]
fn keyboard_reset_identify_and_enable() {
    let mut k = I8042::new();
    data(&mut k, 0xFF);
    assert_eq!(drain(&mut k), [0xFA, 0xAA]);
    data(&mut k, 0xF2);
    assert_eq!(drain(&mut k), [0xFA, 0xAB, 0x83]);
    data(&mut k, 0xF5);
    assert_eq!(drain(&mut k), [0xFA]);
    k.push_scancode(0x1C);
    assert_eq!(drain(&mut k), [], "scanning is disabled");
    data(&mut k, 0xF4);
    assert_eq!(drain(&mut k), [0xFA]);
    k.push_scancode(0x1C);
    assert_eq!(drain(&mut k), [0x1E]);
}

#[test]
fn leds_typematic_and_the_scancode_set() {
    let mut k = I8042::new();
    data(&mut k, 0xED);
    assert_eq!(drain(&mut k), [0xFA]);
    data(&mut k, 0x07);
    assert_eq!(drain(&mut k), [0xFA]);
    assert_eq!(k.leds(), 7);
    data(&mut k, 0xF3);
    data(&mut k, 0x20);
    assert_eq!(drain(&mut k), [0xFA, 0xFA]);
    data(&mut k, 0xF0);
    data(&mut k, 0x00);
    assert_eq!(drain(&mut k), [0xFA, 0xFA, 0x02], "the current set is 2");
    data(&mut k, 0xF0);
    data(&mut k, 0x03);
    assert_eq!(drain(&mut k), [0xFA, 0xFA]);
    assert_eq!(k.scancode_set(), 3);
    data(&mut k, 0xF0);
    data(&mut k, 0x09);
    assert_eq!(drain(&mut k), [0xFA, 0xFE], "a bad set is refused");
    data(&mut k, 0xFF);
    assert_eq!(drain(&mut k), [0xFA, 0xAA]);
    assert_eq!(
        (k.scancode_set(), k.leds()),
        (2, 0),
        "a reset restores the defaults"
    );
}

#[test]
fn echo_resend_and_unknown_commands() {
    let mut k = I8042::new();
    data(&mut k, 0xEE);
    assert_eq!(drain(&mut k), [0xEE]);
    data(&mut k, 0x99);
    assert_eq!(drain(&mut k), [0xFE], "unknown: please resend");
    data(&mut k, 0xF2);
    assert_eq!(rd(&mut k), 0xFA);
    data(&mut k, 0xFE);
    assert_eq!(
        drain(&mut k),
        [0xAB, 0x83, 0xFA],
        "resend repeats the last byte sent"
    );
}

#[test]
fn set_2_is_translated_to_set_1_when_the_command_byte_says_so() {
    let mut k = I8042::new();
    for (set2, set1) in [
        (0x1C, 0x1E),
        (0x1B, 0x1F),
        (0x1A, 0x2C),
        (0x15, 0x10),
        (0x5A, 0x1C),
        (0x29, 0x39),
        (0x76, 0x01),
        (0x66, 0x0E),
        (0x0D, 0x0F),
        (0x12, 0x2A),
        (0x59, 0x36),
        (0x14, 0x1D),
        (0x11, 0x38),
        (0x58, 0x3A),
        (0x05, 0x3B),
        (0x83, 0x41),
    ] {
        k.push_scancode(set2);
        assert_eq!(drain(&mut k), [set1], "set 2 {set2:#x}");
    }
}

#[test]
fn releases_and_extended_keys() {
    let mut k = I8042::new();
    k.push_scancode(0xF0);
    k.push_scancode(0x1C);
    assert_eq!(
        drain(&mut k),
        [0x9E],
        "a release is the make code with bit 7"
    );
    k.push_scancode(0xE0);
    k.push_scancode(0x75); // up arrow
    assert_eq!(drain(&mut k), [0xE0, 0x48]);
    k.push_scancode(0xE0);
    k.push_scancode(0xF0);
    k.push_scancode(0x75);
    assert_eq!(drain(&mut k), [0xE0, 0xC8]);
    k.push_scancode(0x1C);
    assert_eq!(drain(&mut k), [0x1E], "the release prefix applied once");
}

#[test]
fn without_translation_bytes_pass_through() {
    let mut k = I8042::new();
    cmd(&mut k, 0x60);
    data(&mut k, 0x05); // INT + SYS, no translation
    for b in [0x1C, 0xF0, 0x1C] {
        k.push_scancode(b);
    }
    assert_eq!(drain(&mut k), [0x1C, 0xF0, 0x1C]);
}

#[test]
fn the_interrupt_follows_the_output_buffer_and_the_enable_bit() {
    let mut k = I8042::new();
    assert!(!k.irq1());
    k.push_scancode(0x1C);
    assert!(k.irq1());
    rd(&mut k);
    assert!(!k.irq1(), "reading the byte drops the line");
    cmd(&mut k, 0x60);
    data(&mut k, 0x44); // INT off
    k.push_scancode(0x1C);
    assert!(!k.irq1());
    assert_eq!(status(&mut k) & 1, 1, "but the byte is there for polling");
}

#[test]
fn a_read_that_brings_the_next_byte_in_is_a_new_edge() {
    let mut k = I8042::new();
    data(&mut k, 0xF2); // identify: ACK and two ID bytes
    assert!(!k.take_reloaded(), "nothing read yet");
    assert_eq!(k.queued(), 3);
    assert_eq!(rd(&mut k), 0xFA);
    assert_eq!(k.queued(), 2);
    assert!(k.irq1());
    assert!(k.take_reloaded(), "the line dropped and rose with 0xAB");
    assert!(!k.take_reloaded(), "taking it clears it");
    assert_eq!(rd(&mut k), 0xAB);
    assert!(k.take_reloaded());
    assert_eq!(rd(&mut k), 0x83);
    assert!(!k.take_reloaded(), "the last byte leaves the line low");
    assert!(!k.irq1());
    // one byte at a time is a plain rise and fall
    k.push_scancode(0x1C);
    rd(&mut k);
    assert!(!k.take_reloaded());
    // reading an empty buffer reloads nothing
    rd(&mut k);
    assert!(!k.take_reloaded());
}

#[test]
fn set_1_bytes_go_back_to_set_2_and_through_the_translation_unchanged() {
    let mut mapped = 0;
    for code in 1u8..0x80 {
        let Some(s2) = set1_to_set2(code) else {
            continue;
        };
        mapped += 1;
        assert!(s2 < 0x80, "{code:#x}");
        // press and release, as a host forwarding its own keyboard does
        let mut k = I8042::new();
        k.push_scancode(s2);
        k.push_scancode(0xF0);
        k.push_scancode(s2);
        assert_eq!(drain(&mut k), [code, code | 0x80], "{code:#x} via {s2:#x}");
    }
    // every set-1 key code of a PC keyboard has a set-2 key
    assert!(mapped >= 0x58, "{mapped}");
    assert_eq!(set1_to_set2(0x1E), Some(0x1C), "a");
    assert_eq!(set1_to_set2(0x1C), Some(0x5A), "Enter");
    assert_eq!(set1_to_set2(0x01), Some(0x76), "Esc");
    assert_eq!(set1_to_set2(0x80), None);
}

#[test]
fn the_output_buffer_has_sixteen_slots_and_overflow_is_counted() {
    let mut k = I8042::new();
    for _ in 0..QUEUE + 3 {
        k.push_scancode(0x1C);
    }
    assert_eq!(k.dropped, 3);
    assert_eq!(drain(&mut k).len(), QUEUE);
}

#[test]
fn reading_an_empty_output_buffer_repeats_the_last_byte() {
    let mut k = I8042::new();
    k.push_scancode(0x1C);
    assert_eq!(rd(&mut k), 0x1E);
    assert_eq!(rd(&mut k), 0x1E);
    assert_eq!(status(&mut k) & 1, 0);
}

#[test]
fn the_output_port_carries_a20_and_reset() {
    let mut k = I8042::new();
    assert!(k.a20_enabled());
    cmd(&mut k, 0xD0);
    assert_eq!(drain(&mut k), [0x03]);
    cmd(&mut k, 0xD1);
    data(&mut k, 0x01);
    assert!(!k.a20_enabled());
    assert!(!k.take_reset());
    cmd(&mut k, 0xD1);
    data(&mut k, 0x02);
    assert!(k.a20_enabled());
    assert!(k.take_reset(), "bit 0 low pulses reset");
    assert!(!k.take_reset());
    cmd(&mut k, 0xFE);
    assert!(k.take_reset());
    cmd(&mut k, 0xFF);
    assert!(!k.take_reset(), "pulsing no lines resets nothing");
}

#[test]
fn controller_commands_that_feed_the_output_buffer() {
    let mut k = I8042::new();
    cmd(&mut k, 0xD2);
    data(&mut k, 0x55);
    assert_eq!(
        drain(&mut k),
        [0x55],
        "written back as if the keyboard sent it"
    );
    cmd(&mut k, 0xC0);
    assert_eq!(drain(&mut k), [0x80]);
}

#[test]
fn there_is_no_mouse() {
    let mut k = I8042::new();
    cmd(&mut k, 0xA8);
    cmd(&mut k, 0xD4);
    data(&mut k, 0xFF);
    assert_eq!(drain(&mut k), [], "no reply: a probe times out");
    assert_eq!(k.unsupported, 1);
    cmd(&mut k, 0xB5); // an unknown controller command
    assert_eq!(k.unsupported, 2);
}

#[test]
fn a_controller_command_cancels_a_pending_argument() {
    let mut k = I8042::new();
    cmd(&mut k, 0x60);
    cmd(&mut k, 0xAA); // a new command instead of the argument
    assert_eq!(drain(&mut k), [0x55]);
    data(&mut k, 0xEE);
    assert_eq!(
        drain(&mut k),
        [0xEE],
        "this byte is a keyboard command, not the command byte"
    );
}

#[test]
fn in_scancode_set_3_bytes_pass_untranslated() {
    let mut k = I8042::new();
    data(&mut k, 0xF0);
    data(&mut k, 0x03);
    drain(&mut k);
    k.push_scancode(0x1C);
    assert_eq!(drain(&mut k), [0x1C], "translation applies to set 2 only");
}

#[test]
fn leds_have_three_bits_and_set_4_does_not_exist() {
    let mut k = I8042::new();
    data(&mut k, 0xED);
    data(&mut k, 0xFF);
    drain(&mut k);
    assert_eq!(k.leds(), 7);
    data(&mut k, 0xF0);
    data(&mut k, 0x04);
    assert_eq!(drain(&mut k), [0xFA, 0xFE]);
    assert_eq!(k.scancode_set(), 2);
}

#[test]
fn a_keyboard_reset_forgets_a_half_sent_release() {
    let mut k = I8042::new();
    k.push_scancode(0xF0);
    data(&mut k, 0xFF);
    drain(&mut k);
    k.push_scancode(0x1C);
    assert_eq!(drain(&mut k), [0x1E], "a make, not a release");
}
