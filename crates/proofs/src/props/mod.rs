//! The registry of properties.

use crate::Property;

pub mod nir;
pub mod qga;
pub mod sched;
pub mod svc;
pub mod vswitch;
pub mod window;

pub fn all() -> Vec<Property> {
    let mut v = Vec::new();
    v.extend(nir::properties());
    v.extend(svc::properties());
    v.extend(qga::properties());
    v.extend(vswitch::properties());
    v.extend(sched::properties());
    v.extend(window::properties());
    v
}
