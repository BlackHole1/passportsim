//! Radio register pages (FE, FE2, NRX, BB, BLE baseband): store-only, class U, with first touches
//! allowlisted so a strict run never fails on them. The ROM's spin on 0x6003101C is a tripwire in
//! `pemu-hle`, not here.

use super::store_only::StoreOnly;

/// FE, 0x60006000.
pub type FeModel = StoreOnly<super::block::RadioFe, true>;

/// FE2, 0x60005000.
pub type Fe2Model = StoreOnly<super::block::RadioFe2, true>;

/// NRX, 0x6001CC00.
pub type NrxModel = StoreOnly<super::block::RadioNrx, true>;

/// BB, 0x6001D000.
pub type BbModel = StoreOnly<super::block::RadioBb, true>;

/// BLE baseband, around 0x60031000 (UNVERIFIED window).
pub type BleModel = StoreOnly<super::block::RadioBle, true>;

#[cfg(test)]
mod tests {
    use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
    use pemu_core::regstore::Size;
    use pemu_core::time::VTime;

    use super::super::store_only::TouchTag;
    use super::super::{Peripheral, id, lookup};
    use super::*;

    const T: VTime = VTime(9);

    #[test]
    fn identity_and_class() {
        assert_eq!(<BbModel as Peripheral>::ID, id::RADIO_BB);
        assert_eq!(<BbModel as Peripheral>::BASE, 0x6001_D000);
        assert_eq!(<NrxModel as Peripheral>::BASE, 0x6001_CC00);
        assert_eq!(<FeModel as Peripheral>::BASE, 0x6000_6000);
        assert_eq!(<Fe2Model as Peripheral>::BASE, 0x6000_5000);
        assert_eq!(<BleModel as Peripheral>::BASE, 0x6003_1000);
        assert_eq!(
            BbModel::TAG,
            TouchTag {
                periph: id::RADIO_BB,
                allowlisted: true
            }
        );
        assert_eq!(BbModel::default().fidelity(0x54), Fidelity::U);
    }

    #[test]
    fn read_after_write_and_widths() {
        let mut m = BleModel::default();
        let mut l = FidelityLedger::default();
        m.store(0x1C, Size::B4, 0x8765_4321, T, &mut l);
        assert_eq!(m.load(0x1C, Size::B4, T, &mut l), 0x8765_4321);
        assert_eq!(m.load(0x1D, Size::B2, T, &mut l), 0x6543);
        m.store(0x1F, Size::B1, 0x00, T, &mut l);
        assert_eq!(m.load(0x1C, Size::B4, T, &mut l), 0x0065_4321);
        assert_eq!(m.load(0x20, Size::B4, T, &mut l), 0);
        assert_eq!(l.first_touches().len(), 2, "only 0x1C and 0x20, each once");
    }

    /// Boot touches exactly these four radio registers, all from `rtc_sleep_pu`.
    #[test]
    fn boot_radio_touches_are_reported_once_and_allowlisted() {
        let mut bb = BbModel::default();
        let mut nrx = NrxModel::default();
        let mut fe = FeModel::default();
        let mut fe2 = Fe2Model::default();
        let mut l = FidelityLedger::default();
        let boot = [0x6001_D054, 0x6001_CCD4, 0x6000_6090, 0x6000_50F0];
        for addr in boot {
            let (periph, off) = lookup(addr).expect("radio page mapped");
            let val = match periph {
                id::RADIO_BB => {
                    let v = bb.load(off, Size::B4, T, &mut l);
                    bb.store(off, Size::B4, v, T, &mut l);
                    bb.load(off, Size::B4, T, &mut l)
                }
                id::RADIO_NRX => {
                    let v = nrx.load(off, Size::B4, T, &mut l);
                    nrx.store(off, Size::B4, v, T, &mut l);
                    nrx.load(off, Size::B4, T, &mut l)
                }
                id::RADIO_FE => {
                    let v = fe.load(off, Size::B4, T, &mut l);
                    fe.store(off, Size::B4, v, T, &mut l);
                    fe.load(off, Size::B4, T, &mut l)
                }
                id::RADIO_FE2 => {
                    let v = fe2.load(off, Size::B4, T, &mut l);
                    fe2.store(off, Size::B4, v, T, &mut l);
                    fe2.load(off, Size::B4, T, &mut l)
                }
                other => panic!("{addr:#x} decoded to non-radio block {other:?}"),
            };
            assert_eq!(val, 0);
        }
        let touches: Vec<_> = l
            .first_touches()
            .iter()
            .map(|t| (t.periph, t.off, t.access, t.allowlisted))
            .collect();
        assert_eq!(
            touches,
            vec![
                (id::RADIO_BB, 0x54, TouchAccess::Read, true),
                (id::RADIO_NRX, 0xD4, TouchAccess::Read, true),
                (id::RADIO_FE, 0x90, TouchAccess::Read, true),
                (id::RADIO_FE2, 0xF0, TouchAccess::Read, true),
            ]
        );
        assert!(l.first_touches().iter().all(|t: &FirstTouch| t.size == 4));
    }

    #[test]
    fn reset_restores_values_and_keeps_touches() {
        let mut m = FeModel::default();
        let mut l = FidelityLedger::default();
        m.store(0x90, Size::B4, 3, T, &mut l);
        let mut bank = m.bank().clone();
        bank.reset();
        assert_eq!(bank.read(0x90, Size::B4, T, &mut l, FeModel::TAG), 0);
        assert_eq!(l.first_touches().len(), 1);
    }
}
