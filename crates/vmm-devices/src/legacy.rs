//! The small legacy ports a PC guest pokes and probes: system control port A
//! (0x92: the A20 gate and fast reset), the POST/delay port 0x80 and the other
//! DMA page registers (0x80..=0x8F), the FPU error port 0xF0/0xF1, the two
//! 8237 DMA controllers (0x00..=0x0F, 0xC0..=0xDF: registers that read back
//! what was written, no transfers), and the parallel port probe (0x378..=0x37A:
//! no printer, every read is 0xFF). They hold state only so that a read after a
//! write behaves; none of them moves data.

pub const PORT_SYSTEM_A: u16 = 0x92;
pub const PORT_POST: u16 = 0x80;
pub const PORT_FPU_CLEAR: u16 = 0xF0;
pub const PORT_FPU_RESET: u16 = 0xF1;

#[derive(Clone, Debug)]
pub struct Legacy {
    system_a: u8,
    dma_page: [u8; 16],
    dma: [u8; 32],
    reset_requested: bool,
    pub fpu_clears: u32,
    pub unsupported: u32,
}

impl Default for Legacy {
    fn default() -> Self {
        Self::new()
    }
}

impl Legacy {
    pub const fn new() -> Self {
        Self {
            system_a: 0,
            dma_page: [0; 16],
            dma: [0; 32],
            reset_requested: false,
            fpu_clears: 0,
            unsupported: 0,
        }
    }

    pub fn owns(port: u16) -> bool {
        matches!(port, 0x00..=0x0F | 0x80..=0x8F | 0x92 | 0xC0..=0xDF | 0xF0 | 0xF1 | 0x378..=0x37A)
    }

    /// Is the A20 address line enabled (bit 1 of port 0x92)?
    pub fn a20_enabled(&self) -> bool {
        self.system_a & 2 != 0
    }

    /// A write of 1 to bit 0 of port 0x92 asked for a fast reset; reading clears the request.
    pub fn take_reset(&mut self) -> bool {
        core::mem::take(&mut self.reset_requested)
    }

    /// The last value written to the POST port 0x80.
    pub fn post_code(&self) -> u8 {
        self.dma_page[0]
    }

    /// OUT to one of the ports; false if `port` is not claimed here.
    pub fn write(&mut self, port: u16, value: u8) -> bool {
        match port {
            PORT_SYSTEM_A => {
                if value & 1 != 0 {
                    self.reset_requested = true;
                }
                self.system_a = value & 0x02; // bit 0 reads back as zero; bits 7:6 are the activity lights
            }
            0x80..=0x8F => self.dma_page[usize::from(port - 0x80)] = value,
            PORT_FPU_CLEAR | PORT_FPU_RESET => self.fpu_clears += 1,
            0x00..=0x0F => self.dma[usize::from(port)] = value,
            0xC0..=0xDF => self.dma[usize::from(16 + (port - 0xC0) / 2)] = value,
            0x378..=0x37A => {} // no printer: writes go nowhere
            _ => return false,
        }
        true
    }

    /// IN from one of the ports; None if `port` is not claimed here.
    pub fn read(&mut self, port: u16) -> Option<u8> {
        Some(match port {
            PORT_SYSTEM_A => self.system_a,
            0x80..=0x8F => self.dma_page[usize::from(port - 0x80)],
            PORT_FPU_CLEAR | PORT_FPU_RESET => 0xFF,
            0x00..=0x0F => self.dma[usize::from(port)],
            0xC0..=0xDF => {
                // The slave controller's registers are on even ports; odd ones float.
                if port & 1 == 0 {
                    self.dma[usize::from(16 + (port - 0xC0) / 2)]
                } else {
                    0xFF
                }
            }
            0x378..=0x37A => 0xFF,
            _ => return None,
        })
    }
}
