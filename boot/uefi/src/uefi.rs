//! UEFI 2.11 x64 ABI, sections 4, 7, 9, and 13. All table slots are present.
//! Unused function slots are pointer-sized integers; they are never called.
#![allow(dead_code)]

use core::ffi::c_void;
use core::mem::{offset_of, size_of};

pub type Status = usize;
pub type Handle = *mut c_void;
pub const SUCCESS: Status = 0;
pub const INVALID_PARAMETER: Status = (1usize << 63) | 2;
pub const BUFFER_TOO_SMALL: Status = (1usize << 63) | 5;
pub const NOT_FOUND: Status = (1usize << 63) | 14;
pub const SYSTEM_SIGNATURE: u64 = 0x5453_5953_2049_4249;
pub const BOOT_SIGNATURE: u64 = 0x5652_4553_544f_4f42;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);
pub const LOADED_IMAGE: Guid = Guid(
    0x5b1b31a1,
    0x9562,
    0x11d2,
    [0x8e, 0x3f, 0, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
pub const SIMPLE_FS: Guid = Guid(
    0x964e5b22,
    0x6459,
    0x11d2,
    [0x8e, 0x39, 0, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
pub const FILE_INFO: Guid = Guid(
    0x09576e92,
    0x6d3f,
    0x11d2,
    [0x8e, 0x39, 0, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
pub const ACPI_20: Guid = Guid(
    0x8868e871,
    0xe4f1,
    0x11d3,
    [0xbc, 0x22, 0, 0x80, 0xc7, 0x3c, 0x88, 0x81],
);

#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *mut u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut c_void,
    pub standard_error_handle: Handle,
    pub std_err: *mut c_void,
    pub runtime_services: *mut c_void,
    pub boot_services: *const BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *const ConfigurationTable,
}

#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *const c_void,
}

#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,
    pub raise_tpl: usize,
    pub restore_tpl: usize,
    pub allocate_pages: unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(u64, usize) -> Status,
    pub get_memory_map: unsafe extern "efiapi" fn(
        *mut usize,
        *mut c_void,
        *mut usize,
        *mut usize,
        *mut u32,
    ) -> Status,
    pub allocate_pool: usize,
    pub free_pool: usize,
    pub create_event: usize,
    pub set_timer: usize,
    pub wait_for_event: usize,
    pub signal_event: usize,
    pub close_event: usize,
    pub check_event: usize,
    pub install_protocol_interface: usize,
    pub reinstall_protocol_interface: usize,
    pub uninstall_protocol_interface: usize,
    pub handle_protocol: unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> Status,
    pub reserved: usize,
    pub register_protocol_notify: usize,
    pub locate_handle: usize,
    pub locate_device_path: usize,
    pub install_configuration_table: usize,
    pub load_image: usize,
    pub start_image: usize,
    pub exit: usize,
    pub unload_image: usize,
    pub exit_boot_services: unsafe extern "efiapi" fn(Handle, usize) -> Status,
    pub get_next_monotonic_count: usize,
    pub stall: usize,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    pub connect_controller: usize,
    pub disconnect_controller: usize,
    pub open_protocol: usize,
    pub close_protocol: usize,
    pub open_protocol_information: usize,
    pub protocols_per_handle: usize,
    pub locate_handle_buffer: usize,
    pub locate_protocol: usize,
    pub install_multiple_protocol_interfaces: usize,
    pub uninstall_multiple_protocol_interfaces: usize,
    pub calculate_crc32: usize,
    pub copy_mem: usize,
    pub set_mem: usize,
    pub create_event_ex: usize,
}

#[repr(C)]
pub struct LoadedImage {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *const SystemTable,
    pub device_handle: Handle,
    pub file_path: *const c_void,
    pub reserved: *const c_void,
    pub load_options_size: u32,
    pub load_options: *const c_void,
    pub image_base: *const c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    pub unload: usize,
}

#[repr(C)]
pub struct SimpleFileSystem {
    pub revision: u64,
    pub open_volume: unsafe extern "efiapi" fn(*mut Self, *mut *mut File) -> Status,
}

#[repr(C)]
pub struct File {
    pub revision: u64,
    pub open: unsafe extern "efiapi" fn(*mut Self, *mut *mut Self, *const u16, u64, u64) -> Status,
    pub close: unsafe extern "efiapi" fn(*mut Self) -> Status,
    pub delete: usize,
    pub read: unsafe extern "efiapi" fn(*mut Self, *mut usize, *mut c_void) -> Status,
    pub write: usize,
    pub get_position: usize,
    pub set_position: usize,
    pub get_info:
        unsafe extern "efiapi" fn(*mut Self, *const Guid, *mut usize, *mut c_void) -> Status,
    pub set_info: usize,
    pub flush: usize,
    pub open_ex: usize,
    pub read_ex: usize,
    pub write_ex: usize,
    pub flush_ex: usize,
}

#[repr(C)]
pub struct MemoryDescriptor {
    pub kind: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

// Compile-time assertions cover every slot that is dereferenced or called, and
// each complete table size. No Rust-default-layout structure crosses the ABI.
const _: () = {
    assert!(size_of::<usize>() == 8);
    assert!(size_of::<Guid>() == 16);
    assert!(size_of::<TableHeader>() == 24);
    assert!(size_of::<SystemTable>() == 120);
    assert!(offset_of!(SystemTable, boot_services) == 96);
    assert!(offset_of!(SystemTable, number_of_table_entries) == 104);
    assert!(offset_of!(SystemTable, configuration_table) == 112);
    assert!(size_of::<ConfigurationTable>() == 24);
    assert!(offset_of!(ConfigurationTable, vendor_table) == 16);
    assert!(size_of::<BootServices>() == 376);
    assert!(offset_of!(BootServices, allocate_pages) == 40);
    assert!(offset_of!(BootServices, free_pages) == 48);
    assert!(offset_of!(BootServices, get_memory_map) == 56);
    assert!(offset_of!(BootServices, handle_protocol) == 152);
    assert!(offset_of!(BootServices, exit_boot_services) == 232);
    assert!(offset_of!(BootServices, set_watchdog_timer) == 256);
    assert!(size_of::<LoadedImage>() == 96);
    assert!(offset_of!(LoadedImage, device_handle) == 24);
    assert!(size_of::<SimpleFileSystem>() == 16);
    assert!(offset_of!(SimpleFileSystem, open_volume) == 8);
    assert!(size_of::<File>() == 120);
    assert!(offset_of!(File, open) == 8);
    assert!(offset_of!(File, close) == 16);
    assert!(offset_of!(File, read) == 32);
    assert!(offset_of!(File, get_info) == 64);
    assert!(size_of::<MemoryDescriptor>() == 40);
    assert!(offset_of!(MemoryDescriptor, physical_start) == 8);
    assert!(offset_of!(MemoryDescriptor, number_of_pages) == 24);
};
