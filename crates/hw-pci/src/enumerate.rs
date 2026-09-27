//! Depth-bounded bus walk with bus-number validation or assignment.

use crate::{
    read_bridge_buses, read_header, write_bridge_buses, Bdf, BridgeBuses, BusRange, ConfigSpace,
    Header, HeaderKind, PciError,
};

/// Upper bound for [`EnumerationConfig::max_depth`]; it also bounds the
/// recursion depth of [`enumerate`].
pub const MAX_BRIDGE_DEPTH: u8 = 32;

const DEVICES_PER_BUS: u8 = 32;
const FUNCTIONS_PER_DEVICE: u8 = 8;
const BUS_COUNT: usize = 256;
/// `owner` marker: bus is outside the enumerated range.
const OUTSIDE: u8 = 0xFF;
/// `owner` marker: bus has been scanned and can never be claimed again.
const SCANNED: u8 = 0xFE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusNumbering {
    /// Keep the bus numbers firmware programmed and reject inconsistent ones.
    Validate,
    /// Renumber every bridge depth-first from `buses.start() + 1`. Bridges
    /// are expected to be unprogrammed or programmed without overlaps; the
    /// walk does not reset bridges it has not reached yet. On error, bridges
    /// already written keep their new numbers.
    Assign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnumerationConfig {
    /// Bus range of the segment; the walk starts at `buses.start()`.
    pub buses: BusRange,
    /// Maximum bridge nesting: buses behind `max_depth` bridges are scanned,
    /// a bridge that would create a deeper bus is an error.
    pub max_depth: u8,
    pub numbering: BusNumbering,
}

/// One discovered function, in discovery order (parents before children).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Function {
    pub bdf: Bdf,
    pub header: Header,
    /// Number of bridges between the root bus and this function's bus.
    pub depth: u8,
    /// Bridge whose secondary bus holds this function; `None` on the root bus.
    pub parent: Option<Bdf>,
    /// Final bus numbers for PCI bridges.
    pub bridge: Option<BridgeBuses>,
}

/// Walk the segment from its root bus and store every function in `out`.
///
/// Functions 1-7 are probed only when function 0 is multi-function. Every
/// bus is scanned at most once; bridge ranges must nest inside their parent
/// and must not overlap siblings. Returns the number of entries written. On
/// any error, including [`PciError::BufferTooSmall`], nothing is reported as
/// discovered and the contents of `out` are unspecified.
pub fn enumerate<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    config: &EnumerationConfig,
    out: &mut [Function],
) -> Result<usize, PciError> {
    if config.max_depth > MAX_BRIDGE_DEPTH {
        return Err(PciError::InvalidDepthLimit);
    }
    let mut owner = [OUTSIDE; BUS_COUNT];
    for bus in config.buses.start()..=config.buses.end() {
        owner[usize::from(bus)] = 0;
    }
    let mut walker = Walker {
        cfg,
        out,
        count: 0,
        owner,
        config: *config,
        next_bus: u16::from(config.buses.start()) + 1,
    };
    walker.scan_bus(config.buses.start(), 0, None)?;
    Ok(walker.count)
}

struct Walker<'a, C: ConfigSpace + ?Sized> {
    cfg: &'a mut C,
    out: &'a mut [Function],
    count: usize,
    /// Per bus: OUTSIDE, SCANNED, or the nesting level of the deepest bridge
    /// range claiming it (0 = only the root range).
    owner: [u8; BUS_COUNT],
    config: EnumerationConfig,
    /// Assign mode: next unused bus number.
    next_bus: u16,
}

impl<C: ConfigSpace + ?Sized> Walker<'_, C> {
    fn scan_bus(&mut self, bus: u8, depth: u8, parent: Option<Bdf>) -> Result<(), PciError> {
        self.owner[usize::from(bus)] = SCANNED;
        for device in 0..DEVICES_PER_BUS {
            let first = Bdf::new(bus, device, 0)?;
            let Some(header) = read_header(self.cfg, first)? else {
                continue;
            };
            let functions = if header.multi_function {
                FUNCTIONS_PER_DEVICE
            } else {
                1
            };
            self.visit(first, header, depth, parent)?;
            for function in 1..functions {
                let bdf = Bdf::new(bus, device, function)?;
                if let Some(header) = read_header(self.cfg, bdf)? {
                    self.visit(bdf, header, depth, parent)?;
                }
            }
        }
        Ok(())
    }

    fn visit(
        &mut self,
        bdf: Bdf,
        header: Header,
        depth: u8,
        parent: Option<Bdf>,
    ) -> Result<(), PciError> {
        let slot = self.count;
        let entry = self.out.get_mut(slot).ok_or(PciError::BufferTooSmall)?;
        *entry = Function {
            bdf,
            header,
            depth,
            parent,
            bridge: None,
        };
        self.count += 1;
        if header.kind != HeaderKind::PciBridge {
            return Ok(());
        }
        if depth >= self.config.max_depth {
            return Err(PciError::DepthExceeded);
        }
        let buses = match self.config.numbering {
            BusNumbering::Validate => self.descend_existing(bdf, depth)?,
            BusNumbering::Assign => self.descend_assigning(bdf, depth)?,
        };
        if let Some(entry) = self.out.get_mut(slot) {
            entry.bridge = Some(buses);
        }
        Ok(())
    }

    fn descend_existing(&mut self, bdf: Bdf, depth: u8) -> Result<BridgeBuses, PciError> {
        let buses = read_bridge_buses(self.cfg, bdf)?;
        if buses.primary != bdf.bus() {
            return Err(PciError::BridgePrimaryMismatch);
        }
        if buses.subordinate < buses.secondary {
            return Err(PciError::SubordinateBelowSecondary);
        }
        self.claim(buses.secondary, buses.subordinate, depth)?;
        self.scan_bus(buses.secondary, depth + 1, Some(bdf))?;
        Ok(buses)
    }

    /// Mark `secondary..=subordinate` as owned by a bridge on a bus at
    /// `depth`. Every bus must currently belong to the parent range only.
    fn claim(&mut self, secondary: u8, subordinate: u8, depth: u8) -> Result<(), PciError> {
        for bus in secondary..=subordinate {
            match self.owner[usize::from(bus)] {
                OUTSIDE => return Err(PciError::BusOutOfRange),
                SCANNED => return Err(PciError::BusConflict),
                level if level == depth => {}
                level if level < depth => return Err(PciError::BusOutsideWindow),
                _ => return Err(PciError::BusConflict),
            }
        }
        for bus in secondary..=subordinate {
            self.owner[usize::from(bus)] = depth + 1;
        }
        Ok(())
    }

    fn descend_assigning(&mut self, bdf: Bdf, depth: u8) -> Result<BridgeBuses, PciError> {
        let end = self.config.buses.end();
        let secondary = u8::try_from(self.next_bus)
            .ok()
            .filter(|bus| *bus <= end)
            .ok_or(PciError::BusExhausted)?;
        self.next_bus += 1;
        // Open the window to the end of the segment while children are
        // numbered, then close it to the last bus actually used.
        write_bridge_buses(
            self.cfg,
            bdf,
            BridgeBuses {
                primary: bdf.bus(),
                secondary,
                subordinate: end,
            },
        )?;
        self.scan_bus(secondary, depth + 1, Some(bdf))?;
        // next_bus > secondary here, and every assigned bus was <= end.
        let subordinate = u8::try_from(self.next_bus - 1).map_err(|_| PciError::BusExhausted)?;
        let buses = BridgeBuses {
            primary: bdf.bus(),
            secondary,
            subordinate,
        };
        write_bridge_buses(self.cfg, bdf, buses)?;
        Ok(buses)
    }
}
