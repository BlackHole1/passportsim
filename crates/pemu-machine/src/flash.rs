//! A read-only host view of the machine's flash: the base image with the guest's writes over it.
//! `flash save` and the planner's region MD5 read through it. It changes no guest state, so it is
//! not journaled.

use crate::machine::Machine;

impl Machine {
    /// Fills `out` with the flash bytes from `addr` as the part holds them now; bytes past the 8 MB
    /// part read erased.
    pub fn flash_read(&self, addr: u32, out: &mut [u8]) {
        self.soc.flash.read(addr, out);
    }
}

#[cfg(all(test, feature = "bundled-rom"))]
mod tests {
    use pemu_loader::bundle::FlashImage;
    use pemu_loader::efuse_image::EfuseImage;
    use pemu_rv32::bus::{Access, Bus, HartView};

    use crate::config::{Assets, MachineConfig};
    use crate::machine::Machine;

    /// SPI1, the flash command host (TRM SPI chapter).
    const SPI1: u32 = 0x6000_2000;
    const CMD: u32 = SPI1;
    const ADDR: u32 = SPI1 + 0x04;
    const USER: u32 = SPI1 + 0x18;
    const USER1: u32 = SPI1 + 0x1C;
    const USER2: u32 = SPI1 + 0x20;
    const MOSI_DLEN: u32 = SPI1 + 0x24;
    const MISO_DLEN: u32 = SPI1 + 0x28;
    const W0: u32 = SPI1 + 0x58;
    const USR: u32 = 1 << 18;
    const USR_COMMAND: u32 = 1 << 31;
    const USR_ADDR: u32 = 1 << 30;
    const USR_MISO: u32 = 1 << 28;
    const USR_MOSI: u32 = 1 << 27;

    fn machine() -> Machine {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        Machine::new(MachineConfig::default(), assets).expect("the ROM fits")
    }

    fn store(m: &mut Machine, addr: u32, val: u32) {
        let view = HartView {
            insns: 0,
            extra: 0,
            pc: 0,
        };
        m.with_bus(|bus, _| {
            assert!(matches!(
                bus.store_slow(addr, 4, val, &view),
                Access::Ok(()) | Access::OkStop(())
            ));
        });
    }

    fn load(m: &mut Machine, addr: u32) -> u32 {
        let view = HartView {
            insns: 0,
            extra: 0,
            pc: 0,
        };
        m.with_bus(|bus, _| match bus.load_slow(addr, 4, &view) {
            Access::Ok(v) | Access::OkStop(v) => v,
            Access::Fault(_) => panic!("SPI1 is mapped"),
        })
    }

    fn usr(m: &mut Machine, user: u32, opcode: u8, addr: u32, mosi: Option<u32>, miso_bytes: u32) {
        store(m, USER, user);
        store(m, USER1, 23 << 26);
        store(m, USER2, 7 << 28 | u32::from(opcode));
        store(m, ADDR, addr);
        if let Some(word) = mosi {
            store(m, MOSI_DLEN, 4 * 8 - 1);
            store(m, W0, word);
        }
        if miso_bytes > 0 {
            store(m, MISO_DLEN, miso_bytes * 8 - 1);
        }
        store(m, CMD, USR);
    }

    #[test]
    fn spi1_reads_and_programs_reach_the_flash_store_through_the_bus() {
        let mut m = machine();
        store(&mut m, W0, 0);
        usr(
            &mut m,
            USR_COMMAND | USR_ADDR | USR_MISO,
            0x03,
            0x2000,
            None,
            4,
        );
        assert_eq!(load(&mut m, W0), 0xFFFF_FFFF, "an erased part reads 0xFF");

        usr(&mut m, USR_COMMAND, 0x06, 0, None, 0);
        usr(
            &mut m,
            USR_COMMAND | USR_ADDR | USR_MOSI,
            0x02,
            0x1000,
            Some(0x4433_2211),
            0,
        );
        let mut out = [0u8; 6];
        m.flash_read(0x0FFF, &mut out);
        assert_eq!(out, [0xFF, 0x11, 0x22, 0x33, 0x44, 0xFF]);
    }
}
