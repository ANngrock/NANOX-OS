#![no_std]
#![no_main]

mod memory;
mod serial;
mod transition;
mod uefi;

use boot_protocol::{self as bp, BootInfo, LoadedSegment, ReservedRange};
use core::{
    ffi::c_void,
    mem::{align_of, size_of},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};
use memory::{PageTables, Pages};
use uefi::{BootServices, File, Handle, SystemTable};

static TEST_PROFILE: AtomicBool = AtomicBool::new(false);
const ARENA_PAGES: usize = (bp::HANDOFF_MAPPED_SIZE / bp::PAGE_SIZE) as usize;
const RANGES_OFFSET: u64 = 4096;
const SEGMENTS_OFFSET: u64 = 8192;
const MAP_OFFSET: u64 = 16384;
const MAP_CAPACITY: usize = ARENA_PAGES * 4096 - MAP_OFFSET as usize;
const KERNEL_PATH: [u16; 18] = [
    92, 78, 65, 78, 79, 88, 92, 75, 69, 82, 78, 69, 76, 46, 69, 76, 70, 0,
];
const CONFIG_PATH: [u16; 16] = [
    92, 78, 65, 78, 79, 88, 92, 66, 79, 79, 84, 46, 67, 70, 71, 0,
];

pub fn fatal(reason: &str) -> ! {
    serial::write("NANOX:LOADER:ERROR:");
    serial::write(reason);
    serial::write("\n");
    serial::stop(TEST_PROFILE.load(Ordering::Relaxed))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    serial::write("NANOX:LOADER:PANIC\n");
    serial::stop(TEST_PROFILE.load(Ordering::Relaxed))
}

fn aligned<T>(pointer: *const T) -> bool {
    !pointer.is_null() && (pointer as usize).is_multiple_of(align_of::<T>())
}

fn check(status: uefi::Status, reason: &'static str) {
    if status != uefi::SUCCESS {
        serial::write("NANOX:LOADER:STATUS:");
        serial::hex(status as u64);
        serial::write("\n");
        fatal(reason);
    }
}

fn protocol(bs: &BootServices, handle: Handle, guid: &uefi::Guid) -> *mut c_void {
    let mut interface = ptr::null_mut();
    // SAFETY: handle came from the live firmware image or its LoadedImage;
    // GUID and output are aligned live objects, with no output aliases.
    let status = unsafe { (bs.handle_protocol)(handle, guid, &mut interface) };
    check(status, "handle-protocol");
    if interface.is_null() {
        fatal("null-protocol");
    }
    interface
}

fn open_volume(bs: &BootServices, image: Handle) -> *mut File {
    let loaded = protocol(bs, image, &uefi::LOADED_IMAGE).cast::<uefi::LoadedImage>();
    if !aligned(loaded) {
        fatal("loaded-image-alignment");
    }
    // SAFETY: HandleProtocol returned the complete, live LoadedImage object
    // for this known GUID; it is immutable to us and firmware remains active.
    let device = unsafe {
        if (*loaded).revision < 0x1000 {
            fatal("loaded-image-revision");
        }
        (*loaded).device_handle
    };
    if device.is_null() {
        fatal("boot-volume-handle");
    }
    let fs = protocol(bs, device, &uefi::SIMPLE_FS).cast::<uefi::SimpleFileSystem>();
    if !aligned(fs) {
        fatal("filesystem-alignment");
    }
    let mut root = ptr::null_mut();
    // SAFETY: matching GUID provides the two-field protocol. The output root
    // pointer is exclusively borrowed, and no ExitBootServices has occurred.
    let status = unsafe {
        if (*fs).revision < 0x10000 {
            fatal("filesystem-revision");
        }
        ((*fs).open_volume)(fs, &mut root)
    };
    check(status, "open-volume");
    if !aligned(root) {
        fatal("root-file-alignment");
    }
    root
}

fn read_file(
    bs: &BootServices,
    root: *mut File,
    path: &[u16],
    max_size: usize,
    optional: bool,
) -> Option<(Pages, usize)> {
    let mut file = ptr::null_mut();
    // SAFETY: root is an open firmware-owned file handle, path is a static
    // terminated UCS-2 string, output points to an exclusive live pointer.
    let status = unsafe { ((*root).open)(root, &mut file, path.as_ptr(), 1, 0) };
    if optional && status == uefi::NOT_FOUND {
        return None;
    }
    check(status, "open-file");
    if !aligned(file) {
        fatal("file-alignment");
    }
    let mut info = [0u64; 128];
    let mut info_size = size_of_val(&info);
    // SAFETY: GetInfo for FILE_INFO writes only the supplied aligned 1024-byte
    // output, whose size is an in/out live local; the file remains open.
    let status = unsafe {
        ((*file).get_info)(
            file,
            &uefi::FILE_INFO,
            &mut info_size,
            info.as_mut_ptr().cast(),
        )
    };
    check(status, "file-info");
    // EFI_FILE_INFO offsets Size=0, FileSize=8, Attribute=72, FileName=80.
    if info_size < 82
        || info_size > size_of_val(&info)
        || info[0] < 82
        || info[0] > info_size as u64
        || info[9] & 0x10 != 0
    {
        fatal("file-info-layout");
    }
    let len = usize::try_from(info[1]).unwrap_or_else(|_| fatal("file-size-overflow"));
    if len == 0 || len > max_size {
        fatal("file-size-limit");
    }
    let pages = Pages::allocate(bs, len.div_ceil(4096), false);
    let mut done = 0usize;
    while done < len {
        let mut count = len - done;
        // SAFETY: exclusive allocation has at least len bytes; done/count
        // partition its initialized range, and firmware synchronously returns
        // before we read or pass the next disjoint suffix to another call.
        let status =
            unsafe { ((*file).read)(file, &mut count, (pages.base as *mut u8).add(done).cast()) };
        check(status, "file-read");
        if count == 0 || count > len - done {
            fatal("file-short-read");
        }
        done += count;
    }
    let mut extra = 0u8;
    let mut count = 1;
    // SAFETY: one live output byte and size local; file is still open. Reading
    // beyond the reported length detects growth/inconsistent file metadata.
    let status = unsafe { ((*file).read)(file, &mut count, ptr::addr_of_mut!(extra).cast()) };
    check(status, "file-eof");
    if count != 0 {
        fatal("file-grew");
    }
    // SAFETY: file is the open handle owned by this function, used once for
    // Close and never touched afterwards. Firmware is still in boot phase.
    check(unsafe { ((*file).close)(file) }, "close-file");
    Some((pages, len))
}

fn size_of_val<T>(_: &T) -> usize {
    size_of::<T>()
}

fn read_config(bs: &BootServices, root: *mut File) -> (bool, u64) {
    let Some((pages, len)) = read_file(bs, root, &CONFIG_PATH, 24, true) else {
        return (false, 0);
    };
    if len != 24 {
        fatal("config-size");
    }
    // SAFETY: read_file initialized exactly len bytes of a live exclusive
    // allocation. The immutable borrow ends before the allocation is freed.
    let data = unsafe { core::slice::from_raw_parts(pages.base as *const u8, len) };
    if &data[..8] != b"NXCFG001" {
        fatal("config-magic");
    }
    let flags = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let epoch = u64::from_le_bytes(data[16..24].try_into().unwrap());
    if flags & !1 != 0 {
        fatal("config-flags");
    }
    let test = flags == 1;
    pages.free(bs);
    TEST_PROFILE.store(test, Ordering::Relaxed);
    (test, epoch)
}

fn rsdp(system: &SystemTable) -> u64 {
    if system.number_of_table_entries > 1024 {
        fatal("configuration-table-count");
    }
    if system.number_of_table_entries == 0 {
        return 0;
    }
    if !aligned(system.configuration_table) {
        fatal("configuration-table-alignment");
    }
    // SAFETY: firmware entry guarantees this advertised table array remains
    // live before ExitBootServices; count was bounded and elements are aligned.
    let tables = unsafe {
        core::slice::from_raw_parts(system.configuration_table, system.number_of_table_entries)
    };
    tables
        .iter()
        .find(|t| t.vendor_guid == uefi::ACPI_20)
        .map_or(0, |t| t.vendor_table as u64)
}

struct FinalMap {
    len: usize,
    stride: usize,
    version: u32,
}

fn exit_boot_services(bs: &BootServices, image: Handle, arena: Pages) -> FinalMap {
    // Copy only the two permitted entry points into the restricted exit phase.
    // From the first exit attempt onward this function can neither allocate nor
    // reopen files. Retry is bounded, and only a stale-key error is retried.
    let get_map = bs.get_memory_map;
    let exit = bs.exit_boot_services;
    for _ in 0..4 {
        let mut len = MAP_CAPACITY;
        let mut key = 0;
        let mut stride = 0;
        let mut version = 0;
        // SAFETY: map suffix is an exclusive, page-aligned arena allocation
        // with MAP_CAPACITY bytes; output locals are disjoint live objects.
        let status = unsafe {
            get_map(
                &mut len,
                (arena.base + MAP_OFFSET) as *mut c_void,
                &mut key,
                &mut stride,
                &mut version,
            )
        };
        check(status, "final-memory-map");
        if stride < 40
            || stride % 8 != 0
            || len == 0
            || len > MAP_CAPACITY
            || !len.is_multiple_of(stride)
            || version != 1
            || stride > u32::MAX as usize
        {
            fatal("memory-map-layout");
        }
        for offset in (0..len).step_by(stride) {
            // SAFETY: firmware initialized the returned map. Each descriptor
            // begins at its returned stride (not size_of), validated above;
            // the 40-byte prefix lies within len and has eight-byte alignment.
            let descriptor = unsafe {
                &*((arena.base + MAP_OFFSET + offset as u64) as *const uefi::MemoryDescriptor)
            };
            if descriptor.physical_start & 4095 != 0
                || descriptor
                    .number_of_pages
                    .checked_mul(4096)
                    .and_then(|size| descriptor.physical_start.checked_add(size))
                    .is_none()
            {
                fatal("memory-map-range");
            }
        }
        // SAFETY: image is our live image handle and key was obtained above
        // without intervening allocations or firmware calls. No firmware calls
        // follow SUCCESS; INVALID_PARAMETER alone may mean a stale map key.
        let status = unsafe { exit(image, key) };
        if status == uefi::SUCCESS {
            return FinalMap {
                len,
                stride,
                version,
            };
        }
        if status != uefi::INVALID_PARAMETER {
            check(status, "exit-boot-services");
        }
    }
    fatal("exit-boot-services-retry-limit")
}

#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(image: Handle, system: *const SystemTable) -> ! {
    serial::init();
    serial::write("NANOX:LOADER:ENTER\n");
    if image.is_null() || !aligned(system) {
        fatal("firmware-entry");
    }
    // SAFETY: UEFI entry guarantees a live SystemTable. Its fixed header is
    // read first; advertised size/revision must cover every field before use.
    let system = unsafe {
        let hdr = &(*system).hdr;
        if hdr.signature != uefi::SYSTEM_SIGNATURE
            || hdr.header_size < size_of::<SystemTable>() as u32
            || hdr.revision < 0x20000
            || hdr.reserved != 0
        {
            fatal("system-table-header");
        }
        &*system
    };
    if !aligned(system.boot_services) {
        fatal("boot-services-pointer");
    }
    // SAFETY: SystemTable provides a firmware-owned BootServices table; fixed
    // header is validated before constructing a reference to the complete ABI.
    let bs = unsafe {
        let hdr = &(*system.boot_services).hdr;
        if hdr.signature != uefi::BOOT_SIGNATURE
            || hdr.header_size < size_of::<BootServices>() as u32
            || hdr.revision < 0x20000
            || hdr.reserved != 0
        {
            fatal("boot-services-header");
        }
        &*system.boot_services
    };
    // SAFETY: still in boot phase; timeout zero disables the firmware watchdog
    // and a zero-size optional string is represented by a null pointer.
    check(
        unsafe { (bs.set_watchdog_timer)(0, 0, 0, ptr::null()) },
        "disable-watchdog",
    );
    let acpi = rsdp(system);
    let root = open_volume(bs, image);
    let (test, epoch) = read_config(bs, root);
    transition::check_cpu();
    let (file, file_len) = read_file(bs, root, &KERNEL_PATH, 32 * 1024 * 1024, false).unwrap();
    // SAFETY: read_file initialized this live allocation, file_len was bounded
    // to 32 MiB, and the immutable slice expires before free_pages below.
    let bytes = unsafe { core::slice::from_raw_parts(file.base as *const u8, file_len) };
    let elf = bp::elf::parse(bytes).unwrap_or_else(|_| fatal("invalid-elf"));

    let mut ranges = [ReservedRange {
        phys_start: 0,
        page_count: 0,
        kind: 0,
        reserved: 0,
    }; 64];
    let mut range_count = 0usize;
    let mut reserve = |pages: Pages, kind: u32| {
        if range_count == ranges.len() {
            fatal("reserved-range-capacity");
        }
        ranges[range_count] = pages.reserved(kind);
        range_count += 1;
    };
    let arena = Pages::allocate(bs, ARENA_PAGES, false);
    reserve(arena, bp::KIND_BOOT_INFO);
    let stack = Pages::allocate(bs, (bp::STACK_SIZE / 4096) as usize, false);
    reserve(stack, bp::KIND_STACK);
    let mut tables = PageTables::new(bs);
    reserve(tables.pages, bp::KIND_PAGE_TABLES);
    tables.map(bp::HANDOFF_BASE, arena, true, false);
    tables.map(bp::STACK_TOP - bp::STACK_SIZE, stack, true, false);
    // The page below stack is never mapped; no general identity/direct map.
    for (index, segment) in elf.segments().iter().enumerate() {
        let pages = Pages::allocate(bs, segment.page_count() as usize, false);
        reserve(pages, bp::KIND_KERNEL);
        // SAFETY: validated ELF ranges bound file_offset/file_size in bytes;
        // each fresh allocation covers rounded memsz and is disjoint from the
        // file and all prior segments. Zero-filled BSS and page tail persist.
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr().add(segment.file_offset as usize),
                pages.base as *mut u8,
                segment.file_size as usize,
            );
            let loaded = LoadedSegment {
                phys_start: pages.base,
                virt_start: segment.virt_start,
                memory_size: segment.memory_size,
                flags: segment.flags,
                reserved: 0,
            };
            ((arena.base + SEGMENTS_OFFSET) as *mut LoadedSegment)
                .add(index)
                .write(loaded);
        }
        tables.map(
            segment.virt_start,
            pages,
            segment.flags & bp::PF_W != 0,
            segment.flags & bp::PF_X != 0,
        );
    }
    file.free(bs);
    // SAFETY: all child files closed; this root handle is now closed once and
    // never touched again. This is the final filesystem operation.
    check(unsafe { ((*root).close)(root) }, "close-root");
    let transition = transition::Transition::prepare(bs, &mut tables, elf.entry, test);
    reserve(transition.code, bp::KIND_TRANSITION);
    reserve(transition.data, bp::KIND_TRANSITION);
    reserve(transition.stack, bp::KIND_TRANSITION);
    drop(reserve);
    // SAFETY: range count <=64, making the copy <=1536 bytes within its
    // 4096-byte aligned arena region; source stack and arena are disjoint.
    unsafe {
        ptr::copy_nonoverlapping(
            ranges.as_ptr(),
            (arena.base + RANGES_OFFSET) as *mut ReservedRange,
            range_count,
        );
    }
    let mut info = BootInfo::new();
    info.flags = if test { bp::FLAG_TEST_PROFILE } else { 0 };
    info.memory_map_phys = arena.base + MAP_OFFSET;
    info.memory_map_virt = bp::HANDOFF_BASE + MAP_OFFSET;
    info.reserved_ranges_phys = arena.base + RANGES_OFFSET;
    info.reserved_ranges_virt = bp::HANDOFF_BASE + RANGES_OFFSET;
    info.reserved_ranges_count = range_count as u32;
    info.rsdp_phys = acpi;
    info.kernel_entry_virt = elf.entry;
    info.load_segments_phys = arena.base + SEGMENTS_OFFSET;
    info.load_segments_virt = bp::HANDOFF_BASE + SEGMENTS_OFFSET;
    info.load_segments_count = elf.segment_count as u32;
    info.pml4_phys = tables.pages.base;
    info.stack_top_virt = bp::STACK_TOP;
    info.serial_io_port = bp::SERIAL_PORT;
    info.boot_epoch = epoch;
    // No allocations or file operations can occur inside/after this exit path.
    let map = exit_boot_services(bs, image, arena);
    info.memory_map_len = map.len as u64;
    info.memory_descriptor_size = map.stride as u32;
    info.memory_descriptor_version = map.version;
    info.flags |= bp::FLAG_EXIT_BOOT_SERVICES;
    // SAFETY: owned BootInfo arena is still accessible under firmware mappings
    // until CR3 is changed. Offset zero is aligned and has 160 valid bytes;
    // no firmware callback, allocator, or other CPU can access this object.
    unsafe {
        (arena.base as *mut BootInfo).write(info);
    }
    serial::write("NANOX:LOADER:EXIT_BOOT_SERVICES\n");
    transition.enter()
}
