//! Wire formats and pure helpers: status decoding and classes, entry
//! layouts, CAP/doorbell decoding, Identify parsing, PRP list arithmetic.

use hw_nvme::command::{admin, Command, CompletionEntry};
use hw_nvme::identify::IDENTIFY_PARSE_LEN;
use hw_nvme::prp::{self, list_pages_needed, max_data_pages, DataBuffer, PrpList};
use hw_nvme::status::{generic, media};
use hw_nvme::{
    CapError, Capabilities, ControllerInfo, IdentifyError, NamespaceInfo, PrpError, Status,
    StatusClass, StatusCodeType,
};

#[test]
fn status_field_decodes_from_dword3() {
    // SC 81h, SCT 2h, CRD 1, More, DNR, phase 1, CID 0x1234.
    let field: u32 = 0x81 | 2 << 8 | 1 << 11 | 1 << 13 | 1 << 14;
    let dw3 = 0x1234 | 1 << 16 | field << 17;
    let s = Status::from_dw3(dw3);
    assert_eq!(s.sc(), media::UNRECOVERED_READ);
    assert_eq!(s.code_type(), StatusCodeType::MediaDataIntegrity);
    assert_eq!(s.crd(), 1);
    assert!(s.more() && s.dnr() && !s.is_success());
    assert_eq!(s.class(), StatusClass::Media);
    assert_eq!(Status::from_dw3(0x0001_1234), Status::SUCCESS);
}

#[test]
fn status_classes_are_distinct() {
    let g = |sc| Status::new(0, sc, false, false).class();
    assert_eq!(g(0), StatusClass::Success);
    assert_eq!(g(generic::INVALID_OPCODE), StatusClass::InvalidCommand);
    assert_eq!(g(generic::INVALID_PRP_OFFSET), StatusClass::InvalidCommand);
    assert_eq!(g(generic::DATA_TRANSFER_ERROR), StatusClass::DataTransfer);
    assert_eq!(g(generic::ABORT_REQUESTED), StatusClass::Aborted);
    assert_eq!(g(generic::ABORTED_SQ_DELETION), StatusClass::Aborted);
    assert_eq!(g(generic::INTERNAL_ERROR), StatusClass::Internal);
    assert_eq!(g(generic::LBA_OUT_OF_RANGE), StatusClass::OutOfRange);
    assert_eq!(g(generic::NAMESPACE_NOT_READY), StatusClass::NotReady);
    assert_eq!(g(0x20), StatusClass::AccessDenied);
    assert_eq!(g(0x7F), StatusClass::Other);
    assert_eq!(
        Status::new(1, 2, false, false).class(),
        StatusClass::CommandSpecific
    );
    assert_eq!(Status::new(3, 0, false, false).class(), StatusClass::Path);
    assert_eq!(Status::new(7, 0, false, false).class(), StatusClass::Vendor);
    assert_eq!(Status::new(5, 0, false, false).class(), StatusClass::Other);
    assert_eq!(
        Status::new(5, 0, false, false).code_type(),
        StatusCodeType::Reserved(5)
    );
    // SCT 0 SC 0 with DNR is still success by definition of SCT/SC.
    assert!(Status::new(0, 0, false, true).is_success());
}

#[test]
fn entries_round_trip() {
    let mut c = Command::create_io_sq(3, 1023, 0x1234_5000, 2);
    c.set_cid(0xBEEF);
    let back = Command::from_bytes(&c.to_bytes());
    assert_eq!(back, c);
    assert_eq!(back.opcode(), admin::CREATE_IO_SQ);
    assert_eq!(back.cid(), 0xBEEF);
    assert_eq!(back.dw[10], 3 | 1023 << 16);
    assert_eq!(back.dw[11], 1 | 2 << 16);
    assert_eq!(back.prp1(), 0x1234_5000);
    let cq = Command::create_io_cq(1, 7, 0x2000, Some(5));
    assert_eq!(cq.dw[11], 1 | 2 | 5 << 16);
    let r = Command::read(1, 0x1_0000_0002, 7);
    assert_eq!((r.dw[10], r.dw[11], r.dw[12]), (2, 1, 7));
    let e = CompletionEntry {
        dw0: 1,
        dw1: 2,
        sq_head: 3,
        sq_id: 4,
        cid: 5,
        phase: true,
        status: Status::new(1, 2, true, true),
    };
    assert_eq!(CompletionEntry::from_bytes(&e.to_bytes()), e);
}

#[test]
fn capabilities_decode_and_reject() {
    let raw = 0x3FFFu64 | 1 << 16 | 0x0F << 24 | 2 << 32 | 1 << 37 | 4 << 52;
    let cap = Capabilities::parse(raw).unwrap();
    assert_eq!(cap.max_queue_entries(), 0x4000);
    assert!(cap.contiguous_required());
    assert_eq!(cap.ready_timeout_ns(), 15 * 500_000_000);
    assert_eq!(cap.doorbell_stride(), 16);
    assert_eq!(cap.sq_tail_doorbell(0), 0x1000);
    assert_eq!(cap.cq_head_doorbell(0), 0x1010);
    assert_eq!(cap.sq_tail_doorbell(1), 0x1020);
    assert_eq!(cap.cq_head_doorbell(1), 0x1030);
    assert!(cap.nvm_command_set());
    // TO = 0 is floored to one unit instead of an instant timeout.
    let cap0 = Capabilities::parse(raw & !(0xFF << 24)).unwrap();
    assert_eq!(cap0.ready_timeout_ns(), 500_000_000);
    assert_eq!(Capabilities::parse(u64::MAX), Err(CapError::AllOnes));
    assert_eq!(
        Capabilities::parse(raw & !0xFFFF),
        Err(CapError::QueueEntries)
    );
    assert_eq!(
        Capabilities::parse(raw | 5 << 48),
        Err(CapError::PageSizeRange)
    );
    assert_eq!(
        Capabilities::parse(raw & !(0xFF << 37)),
        Err(CapError::NoCommandSet)
    );
    // Largest stride and queue id do not overflow.
    let big = Capabilities::parse(raw | 0xF << 32).unwrap();
    assert_eq!(big.cq_head_doorbell(u16::MAX), 0x1000 + 131_071 * (4 << 15));
}

fn ctrl_data() -> Vec<u8> {
    let mut d = vec![0u8; IDENTIFY_PARSE_LEN];
    d[0..2].copy_from_slice(&0x144Du16.to_le_bytes());
    d[4..24].copy_from_slice(b"SERIAL-SECRET-012345");
    d[77] = 5;
    d[258] = 3;
    d[512] = 0x66;
    d[513] = 0x44;
    d[516] = 1;
    d
}

#[test]
fn identify_controller_parse() {
    let d = ctrl_data();
    let info = ControllerInfo::parse(&d).unwrap();
    assert_eq!(info.vid, 0x144D);
    assert_eq!(info.serial(), b"SERIAL-SECRET-012345");
    assert_eq!(info.max_transfer_bytes(), Some(128 * 1024));
    assert_eq!(info.acl, 3);
    let dbg = format!("{info:?}");
    assert!(!dbg.contains("SECRET"), "{dbg}");
    assert_eq!(
        ControllerInfo::parse(&d[..IDENTIFY_PARSE_LEN - 1]),
        Err(IdentifyError::Truncated)
    );
    let mut bad = d.clone();
    bad[512] = 0x77; // minimum 128 bytes
    assert_eq!(ControllerInfo::parse(&bad), Err(IdentifyError::EntrySize));
    bad[512] = 0x55; // maximum 32 bytes
    assert_eq!(ControllerInfo::parse(&bad), Err(IdentifyError::EntrySize));
    let mut unlimited = d.clone();
    unlimited[77] = 0;
    assert_eq!(
        ControllerInfo::parse(&unlimited)
            .unwrap()
            .max_transfer_bytes(),
        None
    );
    unlimited[77] = 200;
    assert_eq!(
        ControllerInfo::parse(&unlimited)
            .unwrap()
            .max_transfer_bytes(),
        None
    );
}

fn ns_data(nsze: u64, nlbaf: u8, flbas: u8) -> Vec<u8> {
    let mut d = vec![0u8; IDENTIFY_PARSE_LEN];
    d[0..8].copy_from_slice(&nsze.to_le_bytes());
    d[8..16].copy_from_slice(&nsze.to_le_bytes());
    d[25] = nlbaf;
    d[26] = flbas;
    for i in 0..=usize::from(nlbaf) {
        d[128 + 4 * i + 2] = 9 + (i % 4) as u8;
    }
    d
}

#[test]
fn identify_namespace_parse() {
    let ns = NamespaceInfo::parse(&ns_data(1000, 1, 1)).unwrap();
    assert_eq!(ns.lba_size(), 1024);
    assert_eq!(ns.formats().len(), 2);
    // Format index 17 uses FLBAS bits 6:5 for the high bits.
    let ns = NamespaceInfo::parse(&ns_data(1000, 20, 1 | 1 << 5)).unwrap();
    assert_eq!(ns.format_index, 17);
    assert_eq!(ns.lba_shift(), 9 + 17 % 4);
    assert_eq!(
        NamespaceInfo::parse(&ns_data(0, 0, 0)),
        Err(IdentifyError::InactiveNamespace)
    );
    assert_eq!(
        NamespaceInfo::parse(&ns_data(10, 0, 1)),
        Err(IdentifyError::FormatIndex)
    );
    assert_eq!(
        NamespaceInfo::parse(&ns_data(10, 64, 0)),
        Err(IdentifyError::FormatCount)
    );
    let mut d = ns_data(10, 0, 0);
    d[8] = 11;
    assert_eq!(NamespaceInfo::parse(&d), Err(IdentifyError::Capacity));
    let mut d = ns_data(10, 0, 0);
    d[130] = 8;
    assert_eq!(NamespaceInfo::parse(&d), Err(IdentifyError::LbaSize));
    // Size in bytes must fit in 64 bits.
    assert_eq!(
        NamespaceInfo::parse(&ns_data(u64::MAX >> 8, 0, 0)),
        Err(IdentifyError::Capacity)
    );
}

#[test]
fn prp_list_arithmetic() {
    assert_eq!(list_pages_needed(1), 0);
    assert_eq!(list_pages_needed(2), 0);
    assert_eq!(list_pages_needed(3), 1);
    assert_eq!(list_pages_needed(513), 1);
    assert_eq!(list_pages_needed(514), 2);
    assert_eq!(list_pages_needed(1024), 2);
    assert_eq!(list_pages_needed(1025), 3);
    for l in 0..6 {
        let max = max_data_pages(l);
        assert!(list_pages_needed(max) <= u64::from(l));
        assert!(list_pages_needed(max + 1) > u64::from(l));
    }
}

struct Recorder(Vec<(u64, Vec<u8>)>);

impl hw_nvme::DmaMemory for Recorder {
    fn read(&mut self, _: u64, _: &mut [u8]) {}
    fn write(&mut self, pa: u64, data: &[u8]) {
        self.0.push((pa, data.to_vec()));
    }
}

#[test]
fn prp_validation_writes_nothing_on_error() {
    let list = PrpList {
        base: 0x10_0000,
        pages: 1,
    };
    let pages: Vec<u64> = (0..600).map(|i| 0x100_0000 + i * 4096).collect();
    let mut mem = Recorder(Vec::new());
    let cases: [(DataBuffer<'_>, PrpError); 6] = [
        (
            DataBuffer {
                pages: &pages[..1],
                offset: 0,
                len: 0,
            },
            PrpError::Empty,
        ),
        (
            DataBuffer {
                pages: &pages[..1],
                offset: 2,
                len: 4,
            },
            PrpError::OffsetAlignment,
        ),
        (
            DataBuffer {
                pages: &pages[..2],
                offset: 4096,
                len: 4,
            },
            PrpError::OffsetRange,
        ),
        (
            DataBuffer {
                pages: &pages[..3],
                offset: 4,
                len: 4096,
            },
            PrpError::PageCount {
                expected: 2,
                got: 3,
            },
        ),
        (
            DataBuffer {
                pages: &[0x1000, 0x2001],
                offset: 0,
                len: 8192,
            },
            PrpError::PageAlignment(1),
        ),
        (
            DataBuffer {
                pages: &pages[..514],
                offset: 0,
                len: 514 * 4096,
            },
            PrpError::ListTooSmall {
                needed: 2,
                available: 1,
            },
        ),
    ];
    for (buf, err) in cases {
        assert_eq!(prp::build(&mut mem, &buf, list), Err(err));
    }
    assert!(mem.0.is_empty());
    // Misaligned list region is rejected only when a list is needed.
    let bad_list = PrpList {
        base: 0x10_0008,
        pages: 1,
    };
    let two = DataBuffer {
        pages: &pages[..2],
        offset: 0,
        len: 8192,
    };
    assert!(prp::build(&mut mem, &two, bad_list).is_ok());
    let three = DataBuffer {
        pages: &pages[..3],
        offset: 0,
        len: 3 * 4096,
    };
    assert_eq!(
        prp::build(&mut mem, &three, bad_list),
        Err(PrpError::ListRegion)
    );
}

#[test]
fn prp_entries_for_each_shape() {
    let pages: Vec<u64> = (0..1100).map(|i| 0x100_0000 + i * 4096).collect();
    let list = PrpList {
        base: 0x10_0000,
        pages: 3,
    };
    let mut mem = Recorder(Vec::new());
    let one = DataBuffer {
        pages: &pages[..1],
        offset: 512,
        len: 3584,
    };
    let p = prp::build(&mut mem, &one, list).unwrap();
    assert_eq!((p.prp1, p.prp2), (pages[0] + 512, 0));
    let two = DataBuffer {
        pages: &pages[..2],
        offset: 512,
        len: 4096,
    };
    let p = prp::build(&mut mem, &two, list).unwrap();
    assert_eq!((p.prp1, p.prp2), (pages[0] + 512, pages[1]));
    assert!(mem.0.is_empty());
    // 1026 pages: 1025 list entries = 511 + 511 + 3 over three list pages.
    let big = DataBuffer {
        pages: &pages[..1026],
        offset: 0,
        len: 1026 * 4096,
    };
    let p = prp::build(&mut mem, &big, list).unwrap();
    assert_eq!(p.prp2, list.base);
    let mut image = std::collections::BTreeMap::new();
    for (pa, bytes) in &mem.0 {
        for (i, b) in bytes.chunks_exact(8).enumerate() {
            image.insert(pa + 8 * i as u64, u64::from_le_bytes(b.try_into().unwrap()));
        }
    }
    assert_eq!(image[&(list.base + 4088)], list.base + 4096);
    assert_eq!(image[&(list.base + 4096 + 4088)], list.base + 8192);
    assert_eq!(image[&list.base], pages[1]);
    assert_eq!(image[&(list.base + 4080)], pages[511]);
    assert_eq!(image[&(list.base + 4096)], pages[512]);
    assert_eq!(image[&(list.base + 8192 + 16)], pages[1025]);
    assert_eq!(image.len(), 1025 + 2);
}
