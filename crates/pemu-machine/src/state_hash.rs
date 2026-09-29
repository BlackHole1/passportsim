//! `Machine::state_hash`: blake3 over the canonical snapshot sections, so a hash and a snapshot
//! cannot disagree. Sections that record what the host did rather than the guest are left out, so
//! the hash is the same whatever the host did: `host`, the `hostio` read cursors, `hang` (differs
//! with poll fast-forward on or off) and `frame` (numbers the frames a host was shown).

use pemu_core::snap::{HostIoSection, SectionId, SnapHeader, Snapshot};

use crate::machine::Machine;
use crate::snapshot::HOST;

impl Machine {
    /// blake3 over the canonical sections, host-side state excluded.
    pub fn state_hash(&self) -> [u8; 32] {
        let mut snap = Snapshot::new(SnapHeader::new());
        for (id, section) in self.sections() {
            if id.as_str() == HOST
                || id.as_str() == SectionId::HANG
                || id.as_str() == SectionId::FRAME
            {
                continue;
            }
            snap.put_raw(id, section);
        }
        let mut io = HostIoSection::capture(&self.io);
        io.line_read_cursors = Default::default();
        snap.put(&io).expect("the hostio section always encodes");
        snap.canonical_hash()
    }
}
