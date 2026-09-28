//! USB setup packets and standard descriptors (USB 2.0 §9.3, §9.6; USB 3.2
//! §9.6; HID 1.11 §6.2.1).
//!
//! Descriptors come from the device and are untrusted: every length is
//! checked before a field is read, `bLength` must cover the fixed part of
//! the descriptor and stay inside `wTotalLength`, and `wTotalLength` must
//! stay inside the buffer. Unknown (vendor, class, IAD, ...) descriptors
//! are skipped by length.

/// Descriptor types.
pub mod dtype {
    /// DEVICE.
    pub const DEVICE: u8 = 1;
    /// CONFIGURATION.
    pub const CONFIGURATION: u8 = 2;
    /// STRING.
    pub const STRING: u8 = 3;
    /// INTERFACE.
    pub const INTERFACE: u8 = 4;
    /// ENDPOINT.
    pub const ENDPOINT: u8 = 5;
    /// INTERFACE_ASSOCIATION.
    pub const INTERFACE_ASSOCIATION: u8 = 11;
    /// HID (class descriptor).
    pub const HID: u8 = 0x21;
    /// HID report.
    pub const HID_REPORT: u8 = 0x22;
    /// SuperSpeed endpoint companion.
    pub const SS_ENDPOINT_COMPANION: u8 = 0x30;
}

/// Standard request codes (USB 2.0 Table 9-4).
pub mod request {
    /// GET_STATUS.
    pub const GET_STATUS: u8 = 0;
    /// CLEAR_FEATURE.
    pub const CLEAR_FEATURE: u8 = 1;
    /// SET_FEATURE.
    pub const SET_FEATURE: u8 = 3;
    /// SET_ADDRESS.
    pub const SET_ADDRESS: u8 = 5;
    /// GET_DESCRIPTOR.
    pub const GET_DESCRIPTOR: u8 = 6;
    /// GET_CONFIGURATION.
    pub const GET_CONFIGURATION: u8 = 8;
    /// SET_CONFIGURATION.
    pub const SET_CONFIGURATION: u8 = 9;
    /// HID SET_IDLE.
    pub const HID_SET_IDLE: u8 = 0x0A;
    /// HID SET_PROTOCOL.
    pub const HID_SET_PROTOCOL: u8 = 0x0B;
    /// Feature selector ENDPOINT_HALT.
    pub const FEATURE_ENDPOINT_HALT: u16 = 0;
}

/// Descriptor parse errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescError {
    /// Buffer shorter than the fixed part of the descriptor.
    TooShort,
    /// bDescriptorType is not the expected one.
    WrongType,
    /// bLength is smaller than the fixed part or zero/one.
    BadLength,
    /// A descriptor runs past wTotalLength.
    Overrun,
    /// wTotalLength is smaller than the configuration header or larger
    /// than the data received.
    BadTotalLength,
    /// Endpoint descriptor before any interface descriptor.
    EndpointOutsideInterface,
    /// bMaxPacketSize0 / wMaxPacketSize invalid for the speed.
    BadMaxPacket,
    /// bInterval out of range for the speed.
    BadInterval,
}

/// An 8-byte SETUP packet (USB 2.0 §9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPacket {
    /// bmRequestType.
    pub request_type: u8,
    /// bRequest.
    pub request: u8,
    /// wValue.
    pub value: u16,
    /// wIndex.
    pub index: u16,
    /// wLength.
    pub length: u16,
}

impl SetupPacket {
    /// Device-to-host direction bit of bmRequestType.
    pub const DIR_IN: u8 = 0x80;

    /// True when the data stage (if any) is device-to-host.
    pub const fn is_in(&self) -> bool {
        self.request_type & Self::DIR_IN != 0
    }

    /// GET_DESCRIPTOR(`dtype`, `index`) for `length` bytes.
    pub const fn get_descriptor(dtype: u8, index: u8, length: u16) -> Self {
        Self {
            request_type: 0x80,
            request: request::GET_DESCRIPTOR,
            value: (dtype as u16) << 8 | index as u16,
            index: 0,
            length,
        }
    }

    /// SET_CONFIGURATION(`value`).
    pub const fn set_configuration(value: u8) -> Self {
        Self {
            request_type: 0x00,
            request: request::SET_CONFIGURATION,
            value: value as u16,
            index: 0,
            length: 0,
        }
    }

    /// CLEAR_FEATURE(ENDPOINT_HALT) for endpoint address `ep`.
    pub const fn clear_endpoint_halt(ep: u8) -> Self {
        Self {
            request_type: 0x02,
            request: request::CLEAR_FEATURE,
            value: request::FEATURE_ENDPOINT_HALT,
            index: ep as u16,
            length: 0,
        }
    }

    /// HID SET_PROTOCOL (0 = boot, 1 = report) for `interface`.
    pub const fn hid_set_protocol(interface: u8, protocol: u16) -> Self {
        Self {
            request_type: 0x21,
            request: request::HID_SET_PROTOCOL,
            value: protocol,
            index: interface as u16,
            length: 0,
        }
    }

    /// HID SET_IDLE(duration, report 0) for `interface`.
    pub const fn hid_set_idle(interface: u8, duration: u8) -> Self {
        Self {
            request_type: 0x21,
            request: request::HID_SET_IDLE,
            value: (duration as u16) << 8,
            index: interface as u16,
            length: 0,
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

/// Device descriptor (USB 2.0 §9.6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// bcdUSB.
    pub usb_version: u16,
    /// bDeviceClass.
    pub class: u8,
    /// bDeviceSubClass.
    pub subclass: u8,
    /// bDeviceProtocol.
    pub protocol: u8,
    /// bMaxPacketSize0 as stored (an exponent for USB 3.x).
    pub max_packet_size0: u8,
    /// idVendor.
    pub vendor: u16,
    /// idProduct.
    pub product: u16,
    /// bcdDevice.
    pub device_version: u16,
    /// iManufacturer.
    pub manufacturer_index: u8,
    /// iProduct.
    pub product_index: u8,
    /// iSerialNumber.
    pub serial_index: u8,
    /// bNumConfigurations.
    pub num_configurations: u8,
}

/// Length of a device descriptor.
pub const DEVICE_DESCRIPTOR_LEN: usize = 18;

/// EP0 max packet size from the first 8 bytes of a device descriptor
/// for a device at `superspeed` or not.
pub fn ep0_max_packet(prefix: &[u8], superspeed: bool) -> Result<u16, DescError> {
    if prefix.len() < 8 {
        return Err(DescError::TooShort);
    }
    if prefix[1] != dtype::DEVICE {
        return Err(DescError::WrongType);
    }
    if usize::from(prefix[0]) < DEVICE_DESCRIPTOR_LEN {
        return Err(DescError::BadLength);
    }
    let v = prefix[7];
    if superspeed {
        // USB 3.2 §9.6.1: bMaxPacketSize0 is an exponent and must be 9.
        if v == 9 {
            Ok(512)
        } else {
            Err(DescError::BadMaxPacket)
        }
    } else {
        match v {
            8 | 16 | 32 | 64 => Ok(u16::from(v)),
            _ => Err(DescError::BadMaxPacket),
        }
    }
}

impl DeviceDescriptor {
    /// Parses an 18-byte device descriptor.
    pub fn parse(b: &[u8]) -> Result<Self, DescError> {
        if b.len() < DEVICE_DESCRIPTOR_LEN {
            return Err(DescError::TooShort);
        }
        if b[1] != dtype::DEVICE {
            return Err(DescError::WrongType);
        }
        if usize::from(b[0]) < DEVICE_DESCRIPTOR_LEN {
            return Err(DescError::BadLength);
        }
        Ok(Self {
            usb_version: u16_at(b, 2),
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet_size0: b[7],
            vendor: u16_at(b, 8),
            product: u16_at(b, 10),
            device_version: u16_at(b, 12),
            manufacturer_index: b[14],
            product_index: b[15],
            serial_index: b[16],
            num_configurations: b[17],
        })
    }
}

/// Configuration descriptor header (USB 2.0 §9.6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigDescriptor {
    /// wTotalLength.
    pub total_length: u16,
    /// bNumInterfaces.
    pub num_interfaces: u8,
    /// bConfigurationValue.
    pub value: u8,
    /// iConfiguration.
    pub string_index: u8,
    /// bmAttributes.
    pub attributes: u8,
    /// bMaxPower.
    pub max_power: u8,
}

/// Length of a configuration descriptor header.
pub const CONFIG_DESCRIPTOR_LEN: usize = 9;

impl ConfigDescriptor {
    /// Parses the 9-byte header only (used before the full descriptor set
    /// is fetched). Checks bLength and that wTotalLength covers it.
    pub fn parse_header(b: &[u8]) -> Result<Self, DescError> {
        if b.len() < CONFIG_DESCRIPTOR_LEN {
            return Err(DescError::TooShort);
        }
        if b[1] != dtype::CONFIGURATION {
            return Err(DescError::WrongType);
        }
        if usize::from(b[0]) < CONFIG_DESCRIPTOR_LEN {
            return Err(DescError::BadLength);
        }
        let h = Self {
            total_length: u16_at(b, 2),
            num_interfaces: b[4],
            value: b[5],
            string_index: b[6],
            attributes: b[7],
            max_power: b[8],
        };
        if usize::from(h.total_length) < usize::from(b[0]) {
            return Err(DescError::BadTotalLength);
        }
        Ok(h)
    }
}

/// Interface descriptor (USB 2.0 §9.6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    /// bInterfaceNumber.
    pub number: u8,
    /// bAlternateSetting.
    pub alternate: u8,
    /// bNumEndpoints.
    pub num_endpoints: u8,
    /// bInterfaceClass.
    pub class: u8,
    /// bInterfaceSubClass.
    pub subclass: u8,
    /// bInterfaceProtocol.
    pub protocol: u8,
    /// iInterface.
    pub string_index: u8,
}

/// Endpoint transfer type (bmAttributes bits 1:0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferType {
    /// Control.
    Control,
    /// Isochronous.
    Isochronous,
    /// Bulk.
    Bulk,
    /// Interrupt.
    Interrupt,
}

/// Endpoint descriptor (USB 2.0 §9.6.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointDescriptor {
    /// bEndpointAddress.
    pub address: u8,
    /// bmAttributes.
    pub attributes: u8,
    /// wMaxPacketSize (raw, including the HS additional-transactions bits).
    pub max_packet_size: u16,
    /// bInterval.
    pub interval: u8,
}

impl EndpointDescriptor {
    /// Endpoint number 0..=15.
    pub const fn number(&self) -> u8 {
        self.address & 0x0F
    }
    /// True for IN endpoints.
    pub const fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
    /// Transfer type.
    pub const fn transfer_type(&self) -> TransferType {
        match self.attributes & 3 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }
    /// Max packet size in bytes (bits 10:0).
    pub const fn max_packet(&self) -> u16 {
        self.max_packet_size & 0x7FF
    }
    /// HS additional transactions per microframe (bits 12:11).
    pub const fn hs_extra_transactions(&self) -> u8 {
        ((self.max_packet_size >> 11) & 3) as u8
    }
}

/// SuperSpeed endpoint companion (USB 3.2 §9.6.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SsCompanion {
    /// bMaxBurst.
    pub max_burst: u8,
    /// bmAttributes.
    pub attributes: u8,
    /// wBytesPerInterval.
    pub bytes_per_interval: u16,
}

/// HID descriptor (HID 1.11 §6.2.1), fixed part only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidDescriptor {
    /// bcdHID.
    pub version: u16,
    /// bCountryCode.
    pub country: u8,
    /// bNumDescriptors.
    pub num_descriptors: u8,
}

/// One descriptor inside a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Descriptor<'a> {
    /// Interface.
    Interface(InterfaceDescriptor),
    /// Endpoint of the most recent interface.
    Endpoint(EndpointDescriptor),
    /// SuperSpeed companion of the most recent endpoint.
    SsCompanion(SsCompanion),
    /// HID class descriptor.
    Hid(HidDescriptor),
    /// Anything else, skipped by length.
    Other {
        /// bDescriptorType.
        dtype: u8,
        /// Whole descriptor including the header.
        data: &'a [u8],
    },
}

/// Iterator over the descriptors that follow a configuration header.
/// Yields at most one error and then stops.
#[derive(Clone, Debug)]
pub struct ConfigWalker<'a> {
    rest: &'a [u8],
    in_interface: bool,
    failed: bool,
}

/// Parses a full configuration descriptor set: header plus
/// `wTotalLength - bLength` bytes of descriptors. `buf` must hold at least
/// `wTotalLength` bytes; trailing bytes after it are ignored.
pub fn parse_configuration(buf: &[u8]) -> Result<(ConfigDescriptor, ConfigWalker<'_>), DescError> {
    let h = ConfigDescriptor::parse_header(buf)?;
    let total = usize::from(h.total_length);
    if total > buf.len() {
        return Err(DescError::BadTotalLength);
    }
    let start = usize::from(buf[0]);
    Ok((
        h,
        ConfigWalker {
            rest: &buf[start..total],
            in_interface: false,
            failed: false,
        },
    ))
}

impl<'a> ConfigWalker<'a> {
    fn fail(&mut self, e: DescError) -> Option<Result<Descriptor<'a>, DescError>> {
        self.failed = true;
        Some(Err(e))
    }
}

impl<'a> Iterator for ConfigWalker<'a> {
    type Item = Result<Descriptor<'a>, DescError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let r = self.rest;
        if r.len() < 2 {
            return self.fail(DescError::Overrun);
        }
        let len = usize::from(r[0]);
        if len < 2 {
            return self.fail(DescError::BadLength);
        }
        if len > r.len() {
            return self.fail(DescError::Overrun);
        }
        let d = &r[..len];
        self.rest = &r[len..];
        let item = match d[1] {
            dtype::INTERFACE => {
                if len < 9 {
                    return self.fail(DescError::BadLength);
                }
                self.in_interface = true;
                Descriptor::Interface(InterfaceDescriptor {
                    number: d[2],
                    alternate: d[3],
                    num_endpoints: d[4],
                    class: d[5],
                    subclass: d[6],
                    protocol: d[7],
                    string_index: d[8],
                })
            }
            dtype::ENDPOINT => {
                if len < 7 {
                    return self.fail(DescError::BadLength);
                }
                if !self.in_interface {
                    return self.fail(DescError::EndpointOutsideInterface);
                }
                Descriptor::Endpoint(EndpointDescriptor {
                    address: d[2],
                    attributes: d[3],
                    max_packet_size: u16_at(d, 4),
                    interval: d[6],
                })
            }
            dtype::SS_ENDPOINT_COMPANION => {
                if len < 6 {
                    return self.fail(DescError::BadLength);
                }
                Descriptor::SsCompanion(SsCompanion {
                    max_burst: d[2],
                    attributes: d[3],
                    bytes_per_interval: u16_at(d, 4),
                })
            }
            dtype::HID => {
                if len < 6 {
                    return self.fail(DescError::BadLength);
                }
                Descriptor::Hid(HidDescriptor {
                    version: u16_at(d, 2),
                    country: d[4],
                    num_descriptors: d[5],
                })
            }
            t => Descriptor::Other { dtype: t, data: d },
        };
        Some(Ok(item))
    }
}

/// Interrupt IN endpoint of a HID boot keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootKeyboard {
    /// bConfigurationValue to select.
    pub config_value: u8,
    /// Interface number.
    pub interface: u8,
    /// Interrupt IN endpoint.
    pub endpoint: EndpointDescriptor,
    /// SuperSpeed companion, if present.
    pub companion: Option<SsCompanion>,
}

/// Finds the first HID boot keyboard interface (class 3, subclass 1,
/// protocol 1, alternate 0) and its first interrupt IN endpoint. The whole
/// set is validated, so a malformed descriptor after the keyboard is still
/// an error.
pub fn find_boot_keyboard(buf: &[u8]) -> Result<Option<BootKeyboard>, DescError> {
    let (h, walker) = parse_configuration(buf)?;
    let mut found: Option<BootKeyboard> = None;
    let mut in_kbd = false;
    let mut kbd_iface = 0u8;
    let mut last_was_kbd_ep = false;
    for d in walker {
        let d = d?;
        match d {
            Descriptor::Interface(i) => {
                in_kbd = found.is_none()
                    && i.class == 3
                    && i.subclass == 1
                    && i.protocol == 1
                    && i.alternate == 0;
                kbd_iface = i.number;
                last_was_kbd_ep = false;
            }
            Descriptor::Endpoint(e) => {
                last_was_kbd_ep = false;
                if in_kbd
                    && found.is_none()
                    && e.is_in()
                    && e.transfer_type() == TransferType::Interrupt
                {
                    found = Some(BootKeyboard {
                        config_value: h.value,
                        interface: kbd_iface,
                        endpoint: e,
                        companion: None,
                    });
                    last_was_kbd_ep = true;
                }
            }
            Descriptor::SsCompanion(c) => {
                if last_was_kbd_ep {
                    if let Some(f) = found.as_mut() {
                        f.companion = Some(c);
                    }
                }
                last_was_kbd_ep = false;
            }
            _ => {}
        }
    }
    Ok(found)
}
