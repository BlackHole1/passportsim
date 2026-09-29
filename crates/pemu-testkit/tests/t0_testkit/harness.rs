//! `RegHarness` self-tests against the toy peripheral: the scheduler completes a modeled
//! duration, the IRQ stub records the source, the board log records the port call, and the
//! field-name assertions report the block, the register, the field and both values.

use pemu_core::sched::{EventKey, Owner, PeriphId};
use pemu_core::time::VTime;
use pemu_testkit::mock_board::{BoardCall, MockBoard};
use pemu_testkit::reg_harness::{FieldError, FieldMismatch, RegHarness, check_field, read_field};

use crate::toy::{self, Toy};

/// `ENABLE` plus `UPDATE` with `DIV` = 3.
const START: u32 = (1 << 31) | (3 << 2) | 1;

#[test]
fn t0_a_modeled_duration_completes_through_the_scheduler() {
    let mut h = RegHarness::new();
    let mut toy = Toy::new();

    toy.write(&mut h.cx(), toy::CONF_OFF, START);

    // The value is not published at the write: it is an event three DIV steps ahead.
    h.assert_field(toy::BLOCK, &toy.regs, "STATUS", "VALUE_VALID", 0);
    assert_eq!(h.sched.len(), 1);
    assert_eq!(
        h.sched.next_time(),
        Some(VTime(3 * toy::PS_PER_DIV)),
        "the completion is 3 DIV steps ahead"
    );

    // Halfway there, nothing has happened yet.
    let due = h.advance_by(toy::PS_PER_DIV);
    assert!(due.is_empty());
    h.assert_field(toy::BLOCK, &toy.regs, "STATUS", "VALUE_VALID", 0);

    // At the deadline the event is delivered, and the model publishes on it.
    let due = h.advance_to_next_event().expect("an event is pending");
    assert_eq!(
        due,
        vec![EventKey {
            owner: Owner::Periph(PeriphId(0x709)),
            tag: toy::TAG_DONE,
        }]
    );
    for key in due {
        toy.on_event(&mut h.cx(), key.tag);
    }

    h.assert_field(toy::BLOCK, &toy.regs, "STATUS", "VALUE_VALID", 1);
    h.assert_field(toy::BLOCK, &toy.regs, "STATUS", "VALUE", 0x5A);
    assert!(h.irq.level(toy::SOURCE));
    assert_eq!(h.irq.changes().len(), 1);
    assert_eq!(h.irq.changes()[0].t, VTime(3 * toy::PS_PER_DIV));
    assert_eq!(h.irq.high(), vec![toy::SOURCE]);

    // The guest acknowledges: W1C clears DONE, the RO valid flag stays, the source drops.
    toy.ack(&mut h.cx());
    h.assert_field(toy::BLOCK, &toy.regs, "STATUS", "DONE", 0);
    h.assert_field(toy::BLOCK, &toy.regs, "STATUS", "VALUE_VALID", 1);
    assert!(!h.irq.level(toy::SOURCE));
    assert_eq!(h.irq.changes().len(), 2);
}

#[test]
fn t0_the_harness_board_records_the_port_call_of_a_model() {
    let mut h = RegHarness::with_board(MockBoard::new());
    let mut toy = Toy::new();

    h.advance_to(VTime::from_us(10));
    toy.write(&mut h.cx(), toy::CONF_OFF, START);
    toy.write(&mut h.cx(), toy::CONF_OFF, 0);

    assert_eq!(h.board.port_sequence(), vec!["gpio_out", "gpio_out"]);
    assert_eq!(
        h.board.log()[0],
        BoardCall::GpioOut {
            t: VTime::from_us(10),
            pin: 7,
            level: true,
            oe: true,
        }
    );
    assert_eq!(
        h.board.log()[1],
        BoardCall::GpioOut {
            t: VTime::from_us(10),
            pin: 7,
            level: false,
            oe: true,
        }
    );
}

#[test]
fn t0_a_field_read_uses_the_spec_shift_and_width() {
    let mut h = RegHarness::new();
    let mut toy = Toy::new();
    toy.write(&mut h.cx(), toy::CONF_OFF, (0x2A << 2) | 1);

    assert_eq!(read_field(toy::BLOCK, &toy.regs, "CONF", "DIV"), Ok(0x2A));
    assert_eq!(read_field(toy::BLOCK, &toy.regs, "CONF", "ENABLE"), Ok(1));
    // A WT field reads 0 through the bus, and the field helper reads the stored value, which
    // is 0 here as well because nothing wrote UPDATE.
    assert_eq!(read_field(toy::BLOCK, &toy.regs, "CONF", "UPDATE"), Ok(0));
    assert_eq!(toy.read(toy::CONF_OFF), (0x2A << 2) | 1);

    // A field assertion never disturbs the register it inspects, which a bus read would.
    toy.write(&mut h.cx(), toy::CONF_OFF, (1 << 31) | (2 << 2) | 1);
    assert_eq!(read_field(toy::BLOCK, &toy.regs, "CONF", "DIV"), Ok(2));
    assert_eq!(read_field(toy::BLOCK, &toy.regs, "CONF", "DIV"), Ok(2));
}

#[test]
fn t0_a_field_mismatch_names_the_block_register_field_and_both_values() {
    let toy = Toy::new();
    let err = check_field(toy::BLOCK, &toy.regs, "CONF", "DIV", 7)
        .expect_err("the reset value of DIV is 1, not 7");
    let mismatch = err.expect("a mismatch, not a lookup error");
    assert_eq!(
        mismatch,
        FieldMismatch {
            block: "TOY".to_string(),
            reg: "CONF",
            field: "DIV",
            shift: 2,
            width: 6,
            expected: 7,
            actual: 1,
        }
    );
    assert_eq!(
        mismatch.to_string(),
        "TOY.CONF.DIV (bits 2..7): expected 0x7, got 0x1"
    );
}

#[test]
fn t0_an_unknown_name_is_a_lookup_error_that_lists_what_the_block_has() {
    let toy = Toy::new();

    let Err(FieldError::NoRegister { block, reg, known }) =
        read_field(toy::BLOCK, &toy.regs, "CONFIG", "DIV")
    else {
        panic!("a typo in a register name must not read a field");
    };
    assert_eq!((block.as_str(), reg.as_str()), ("TOY", "CONFIG"));
    assert_eq!(known, vec!["CONF", "STATUS"]);

    let Err(err @ FieldError::NoField { .. }) =
        read_field(toy::BLOCK, &toy.regs, "CONF", "DIVIDER")
    else {
        panic!("a typo in a field name must not read a field");
    };
    assert_eq!(
        err.to_string(),
        "TOY.CONF has no field DIVIDER; it has ENABLE, DIV, UPDATE"
    );
}

#[test]
fn t0_virtual_time_never_moves_backwards() {
    let mut h = RegHarness::new();
    h.advance_to(VTime::from_ms(5));
    h.advance_to(VTime::from_ms(1));
    assert_eq!(h.now, VTime::from_ms(5));
}

#[test]
fn t0_an_event_that_is_due_at_the_same_instant_is_delivered_in_insertion_order() {
    let mut h = RegHarness::new();
    let owner = Owner::Periph(PeriphId(1));
    let at = VTime::from_us(1);
    for tag in 0..3u16 {
        h.sched.schedule(h.now, at, EventKey { owner, tag });
    }
    let due = h.advance_to(at);
    assert_eq!(
        due.iter().map(|k| k.tag).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "delivery order is (time, seq)"
    );
}
