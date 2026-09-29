//! The NTAG213 memory, the eight commands and the NDEF helpers.

use pemu_board::ntag213::ndef::{self, NdefRecord};
use pemu_board::ntag213::{
    ACK, CardError, CardResponse, MEM_BYTES, Nak, Ntag213, PAGE_CFG0, PAGE_CFG1, PAGE_DYN_LOCK,
    PAGE_MAX, PAGE_PACK, PAGE_PWD, VERSION, cmd,
};
use pemu_board::passport::{BoardConfig, PassportBoard};
use pemu_board::traits::BoardCx;
use pemu_core::input::{InputEvent, NfcOp};
use pemu_core::rng::DetRng;
use pemu_core::time::VTime;

fn tapped() -> Ntag213 {
    let mut card = Ntag213::new(&mut DetRng::new(7));
    card.field_on();
    card
}

/// 45 pages, the NXP manufacturer byte, both check bytes, the capability container and the Lock
/// Control plus empty NDEF TLVs.
#[test]
fn the_card_starts_in_the_factory_delivery_state() {
    let card = Ntag213::new(&mut DetRng::new(7));
    assert_eq!(card.image().len(), MEM_BYTES);
    assert_eq!(MEM_BYTES, 180);

    let uid = card.uid();
    assert_eq!(uid[0], 0x04, "SN0 is the NXP manufacturer byte");
    let page0 = card.page(0).unwrap();
    assert_eq!(page0[3], 0x88 ^ uid[0] ^ uid[1] ^ uid[2], "BCC0");
    let page2 = card.page(2).unwrap();
    assert_eq!(page2[0], uid[3] ^ uid[4] ^ uid[5] ^ uid[6], "BCC1");
    assert_eq!(page2[2..4], [0x00, 0x00], "static locks clear at delivery");

    assert_eq!(card.page(3).unwrap(), [0xE1, 0x10, 0x12, 0x00], "CC");
    assert_eq!(card.page(4).unwrap(), [0x01, 0x03, 0xA0, 0x0C]);
    assert_eq!(card.page(5).unwrap(), [0x34, 0x03, 0x00, 0xFE]);
    assert_eq!(card.auth0(), 0xFF, "no page is password protected");
    assert_eq!(card.access(), 0x00);
    assert_eq!(card.counter(), 0);
    assert!(card.page(PAGE_MAX + 1).is_none());
}

/// Two seeds give two cards; the same seed gives the same card.
#[test]
fn the_uid_and_signature_follow_the_machine_seed() {
    let a = Ntag213::new(&mut DetRng::new(1));
    let b = Ntag213::new(&mut DetRng::new(2));
    let a_again = Ntag213::new(&mut DetRng::new(1));
    assert_eq!(a.uid(), a_again.uid());
    assert_ne!(a.uid(), b.uid());
    assert_eq!(a.image(), a_again.image());
}

#[test]
fn get_version_and_read_sig_answer_their_fixed_lengths() {
    let mut card = tapped();
    assert_eq!(
        card.command(&[cmd::GET_VERSION]),
        CardResponse::Data(VERSION.to_vec())
    );
    match card.command(&[cmd::READ_SIG, 0x00]) {
        CardResponse::Data(signature) => assert_eq!(signature.len(), 32),
        other => panic!("READ_SIG answered {other:?}"),
    }
    // READ_SIG with any argument but 0 is a malformed frame.
    assert_eq!(
        card.command(&[cmd::READ_SIG, 0x01]),
        CardResponse::Nak(Nak::InvalidArgument)
    );
}

/// READ 2Ah returns pages 2A, 2B, 2C, 00.
#[test]
fn read_returns_four_pages_with_roll_over_and_masks_the_password() {
    let mut card = tapped();
    let CardResponse::Data(bytes) = card.command(&[cmd::READ, PAGE_CFG1]) else {
        panic!("READ should answer data")
    };
    assert_eq!(bytes.len(), 16);
    assert_eq!(&bytes[0..4], card.page(PAGE_CFG1).unwrap());
    assert_eq!(&bytes[4..8], [0, 0, 0, 0], "PWD reads as zeros");
    assert_eq!(&bytes[8..10], [0, 0], "PACK reads as zeros");
    assert_eq!(&bytes[12..16], card.page(0).unwrap(), "roll-over to page 0");

    assert_eq!(
        card.command(&[cmd::READ, PAGE_MAX + 1]),
        CardResponse::Nak(Nak::InvalidArgument)
    );
}

#[test]
fn fast_read_returns_the_requested_range() {
    let mut card = tapped();
    let CardResponse::Data(bytes) = card.command(&[cmd::FAST_READ, 0x04, 0x06]) else {
        panic!("FAST_READ should answer data")
    };
    assert_eq!(bytes.len(), 12);
    assert_eq!(&bytes[0..4], card.page(4).unwrap());
    assert_eq!(
        card.command(&[cmd::FAST_READ, 0x06, 0x04]),
        CardResponse::Nak(Nak::InvalidArgument)
    );
    assert_eq!(
        card.command(&[cmd::FAST_READ, 0x04, PAGE_MAX + 1]),
        CardResponse::Nak(Nak::InvalidArgument)
    );
}

#[test]
fn write_stores_a_user_page_and_comp_write_keeps_only_four_bytes() {
    let mut card = tapped();
    assert_eq!(
        card.command(&[cmd::WRITE, 0x10, 0xDE, 0xAD, 0xBE, 0xEF]),
        CardResponse::Ack
    );
    assert_eq!(card.page(0x10).unwrap(), [0xDE, 0xAD, 0xBE, 0xEF]);

    for page in [0x00u8, 0x01] {
        assert_eq!(
            card.command(&[cmd::WRITE, page, 1, 2, 3, 4]),
            CardResponse::Nak(Nak::InvalidArgument),
            "page {page} is the UID and is read-only"
        );
    }

    assert_eq!(card.command(&[cmd::COMP_WRITE, 0x11]), CardResponse::Ack);
    let mut frame = vec![cmd::COMP_WRITE, 0x11];
    frame.extend_from_slice(&[0xAA; 16]);
    assert_eq!(card.command(&frame), CardResponse::Ack);
    assert_eq!(card.page(0x11).unwrap(), [0xAA; 4]);
    assert_eq!(
        card.page(0x12).unwrap(),
        [0x00; 4],
        "only one page is stored"
    );
}

#[test]
fn the_otp_pages_are_or_ed_and_never_cleared() {
    let mut card = tapped();
    card.command(&[cmd::WRITE, 0x03, 0x00, 0x00, 0x00, 0x0F]);
    assert_eq!(card.page(3).unwrap(), [0xE1, 0x10, 0x12, 0x0F]);
    card.command(&[cmd::WRITE, 0x03, 0x00, 0x00, 0x00, 0x00]);
    assert_eq!(card.page(3).unwrap(), [0xE1, 0x10, 0x12, 0x0F], "OR only");

    // Page 2: bytes 0 and 1 are ignored, the lock bytes are OR-ed.
    card.command(&[cmd::WRITE, 0x02, 0xFF, 0xFF, 0x08, 0x00]);
    let page2 = card.page(2).unwrap();
    assert_ne!(page2[0], 0xFF, "BCC1 is not writable");
    assert_eq!(page2[2], 0x08, "L3 set");
    assert!(card.is_locked(3));
    assert_eq!(
        card.command(&[cmd::WRITE, 0x03, 0, 0, 0, 0xF0]),
        CardResponse::Nak(Nak::WriteError),
        "a locked page refuses the write"
    );

    card.command(&[cmd::WRITE, PAGE_DYN_LOCK, 0x01, 0x00, 0x00, 0x00]);
    assert!(card.is_locked(16) && card.is_locked(17));
    assert!(!card.is_locked(18));
}

#[test]
fn pwd_auth_answers_pack_and_counts_failures_against_authlim() {
    let mut card = tapped();
    // Set PACK and an AUTHLIM of 2, then re-enter the field so the configuration applies.
    card.command(&[cmd::WRITE, PAGE_PACK, 0x12, 0x34, 0x00, 0x00]);
    card.command(&[cmd::WRITE, PAGE_CFG1, 0x02, 0x00, 0x00, 0x00]);
    card.field_off();
    card.field_on();
    assert_eq!(card.authlim(), 2);

    assert_eq!(
        card.command(&[cmd::PWD_AUTH, 0x00, 0x00, 0x00, 0x00]),
        CardResponse::Nak(Nak::InvalidArgument)
    );
    assert_eq!(
        card.command(&[cmd::PWD_AUTH, 0x00, 0x00, 0x00, 0x00]),
        CardResponse::Nak(Nak::AuthOverflow)
    );
    assert_eq!(
        card.command(&[cmd::PWD_AUTH, 0xFF, 0xFF, 0xFF, 0xFF]),
        CardResponse::Nak(Nak::AuthOverflow)
    );

    // Re-entering the field does not give the guesses back: a limit an attacker clears by pulling
    // the phone away is no limit at all.
    card.field_off();
    card.field_on();
    assert_eq!(
        card.command(&[cmd::PWD_AUTH, 0xFF, 0xFF, 0xFF, 0xFF]),
        CardResponse::Nak(Nak::AuthOverflow)
    );
    assert!(!card.authenticated());
}

/// Below the limit the counter accumulates across taps; only a successful PWD_AUTH clears it.
#[test]
fn authlim_counts_across_taps_and_only_a_correct_password_clears_it() {
    let mut card = tapped();
    card.command(&[cmd::WRITE, PAGE_PACK, 0x12, 0x34, 0x00, 0x00]);
    card.command(&[cmd::WRITE, PAGE_CFG1, 0x03, 0x00, 0x00, 0x00]);
    card.field_off();

    // Two taps of one wrong guess each: three would reach the limit, so the tag is still open.
    for _ in 0..2 {
        card.field_on();
        assert_eq!(
            card.command(&[cmd::PWD_AUTH, 0x00, 0x00, 0x00, 0x00]),
            CardResponse::Nak(Nak::InvalidArgument)
        );
        card.field_off();
    }

    card.field_on();
    assert_eq!(
        card.command(&[cmd::PWD_AUTH, 0xFF, 0xFF, 0xFF, 0xFF]),
        CardResponse::Data(vec![0x12, 0x34])
    );
    card.field_off();

    card.field_on();
    for _ in 0..2 {
        assert_eq!(
            card.command(&[cmd::PWD_AUTH, 0x00, 0x00, 0x00, 0x00]),
            CardResponse::Nak(Nak::InvalidArgument)
        );
    }
    assert_eq!(
        card.command(&[cmd::PWD_AUTH, 0x00, 0x00, 0x00, 0x00]),
        CardResponse::Nak(Nak::AuthOverflow)
    );
    card.field_off();
    card.field_on();
    assert_eq!(
        card.command(&[cmd::PWD_AUTH, 0xFF, 0xFF, 0xFF, 0xFF]),
        CardResponse::Nak(Nak::AuthOverflow),
        "the lockout survives the tap that caused it"
    );
}

/// Authenticating lifts both.
#[test]
fn auth0_protects_writes_always_and_reads_only_with_prot() {
    let mut open = tapped();
    open.command(&[cmd::WRITE, PAGE_CFG0, 0x04, 0x00, 0x00, 0x10]);
    assert_eq!(open.auth0(), 0x10);
    assert!(!open.prot());
    assert!(matches!(
        open.command(&[cmd::READ, 0x10]),
        CardResponse::Data(_)
    ));
    assert_eq!(
        open.command(&[cmd::WRITE, 0x10, 1, 2, 3, 4]),
        CardResponse::Nak(Nak::InvalidArgument)
    );

    // CFG1 is written before CFG0, because the configuration pages are above AUTH0 once it is
    // lowered.
    let mut closed = tapped();
    closed.command(&[cmd::WRITE, PAGE_CFG1, 0x80, 0x00, 0x00, 0x00]);
    closed.command(&[cmd::WRITE, PAGE_CFG0, 0x04, 0x00, 0x00, 0x10]);
    assert!(closed.prot());
    assert_eq!(
        closed.command(&[cmd::READ, 0x10]),
        CardResponse::Nak(Nak::InvalidArgument)
    );
    assert!(matches!(
        closed.command(&[cmd::READ, 0x0C]),
        CardResponse::Data(_)
    ));

    closed.command(&[cmd::PWD_AUTH, 0xFF, 0xFF, 0xFF, 0xFF]);
    assert!(closed.authenticated());
    assert!(matches!(
        closed.command(&[cmd::READ, 0x10]),
        CardResponse::Data(_)
    ));
    assert_eq!(
        closed.command(&[cmd::WRITE, 0x10, 1, 2, 3, 4]),
        CardResponse::Ack
    );
}

/// A start-only test is not protection: `FAST_READ 04 27` would return the whole user area, and
/// `READ 0x0F` returns pages 0x0F to 0x12, three above an AUTH0 of 0x10.
#[test]
fn a_read_that_starts_below_auth0_cannot_walk_past_it() {
    let mut card = tapped();
    card.command(&[cmd::WRITE, PAGE_CFG1, 0x80, 0x00, 0x00, 0x00]);
    card.command(&[cmd::WRITE, PAGE_CFG0, 0x04, 0x00, 0x00, 0x12]);
    assert!(card.prot());
    assert_eq!(card.auth0(), 0x12);
    assert!(!card.authenticated());

    assert_eq!(
        card.command(&[cmd::FAST_READ, 0x04, 0x27]),
        CardResponse::Nak(Nak::InvalidArgument),
        "FAST_READ must not return the protected end of its range"
    );
    assert_eq!(
        card.command(&[cmd::FAST_READ, 0x11, 0x12]),
        CardResponse::Nak(Nak::InvalidArgument),
        "one protected page in the range refuses the whole command"
    );
    assert_eq!(
        card.command(&[cmd::READ, 0x10]),
        CardResponse::Nak(Nak::InvalidArgument),
        "READ rolls three pages past its address"
    );

    let CardResponse::Data(bytes) = card.command(&[cmd::FAST_READ, 0x04, 0x11]) else {
        panic!("a range wholly below AUTH0 is readable")
    };
    assert_eq!(bytes.len(), (0x11 - 0x04 + 1) * 4);
    assert!(matches!(
        card.command(&[cmd::READ, 0x0E]),
        CardResponse::Data(_)
    ));

    card.command(&[cmd::PWD_AUTH, 0xFF, 0xFF, 0xFF, 0xFF]);
    assert!(card.authenticated());
    let CardResponse::Data(bytes) = card.command(&[cmd::FAST_READ, 0x04, 0x27]) else {
        panic!("authentication lifts the protection")
    };
    assert_eq!(bytes.len(), 144);
}

/// READ_CNT reports the counter least significant byte first (UNVERIFIED order).
#[test]
fn the_counter_increments_once_per_field_pass_when_enabled() {
    let mut card = tapped();
    card.command(&[cmd::READ, 0x04]);
    card.command(&[cmd::READ, 0x04]);
    assert_eq!(card.counter(), 0, "disabled by default");

    card.command(&[cmd::WRITE, PAGE_CFG1, 0x10, 0x00, 0x00, 0x00]);
    assert!(card.counter_enabled());
    card.field_off();

    for pass in 1..=3u32 {
        card.field_on();
        card.command(&[cmd::READ, 0x04]);
        card.command(&[cmd::FAST_READ, 0x04, 0x05]);
        assert_eq!(card.counter(), pass);
        card.field_off();
    }

    card.field_on();
    assert_eq!(
        card.command(&[cmd::READ_CNT, 0x02]),
        CardResponse::Data(vec![0x03, 0x00, 0x00])
    );
    assert_eq!(
        card.command(&[cmd::READ_CNT, 0x00]),
        CardResponse::Nak(Nak::InvalidArgument)
    );
}

#[test]
fn a_malformed_frame_is_a_nak() {
    let mut card = tapped();
    for frame in [
        vec![],
        vec![0x00],
        vec![cmd::READ],
        vec![cmd::WRITE, 0x10, 1, 2],
        vec![cmd::PWD_AUTH, 1, 2],
    ] {
        assert_eq!(
            card.command(&frame),
            CardResponse::Nak(Nak::InvalidArgument),
            "frame {frame:?}"
        );
    }
    assert_eq!(CardResponse::Ack.to_bytes(), vec![ACK]);
    assert_eq!(
        CardResponse::Nak(Nak::WriteError).to_bytes(),
        vec![Nak::WriteError.code()]
    );
}

/// A reset card is still the same card, so `nfc.reset()` keeps the UID.
#[test]
fn load_takes_an_image_and_reset_keeps_the_uid() {
    let mut card = Ntag213::new(&mut DetRng::new(3));
    let uid = card.uid();
    assert_eq!(card.load(&[0u8; 8]), Err(CardError::ImageLength(8)));

    let mut image = card.image().to_vec();
    image[0x10 * 4] = 0x5A;
    assert_eq!(card.load(&image), Ok(()));
    assert_eq!(card.page(0x10).unwrap()[0], 0x5A);

    card.reset_card();
    assert_eq!(card.uid(), uid);
    assert_eq!(card.page(0x10).unwrap(), [0; 4]);
    assert_eq!(card.page(3).unwrap(), [0xE1, 0x10, 0x12, 0x00]);
}

/// The delivery state holds an empty NDEF TLV behind the Lock Control TLV, and writing a message
/// keeps the Lock Control TLV where it is.
#[test]
fn ndef_round_trips_through_the_tlv_area() {
    let mut card = Ntag213::new(&mut DetRng::new(5));
    assert_eq!(
        card.ndef_read(),
        Ok(vec![]),
        "the delivery NDEF TLV is empty"
    );

    let records = vec![
        NdefRecord::Uri("https://example.com/p".to_string()),
        NdefRecord::Text {
            lang: "en".to_string(),
            text: "hi".to_string(),
        },
    ];
    assert_eq!(card.ndef_write(&records), Ok(()));
    assert_eq!(card.ndef_read(), Ok(records));

    assert_eq!(card.page(4).unwrap(), [0x01, 0x03, 0xA0, 0x0C]);
    assert_eq!(card.page(5).unwrap()[0], 0x34);
    assert_eq!(card.page(5).unwrap()[1], ndef::TLV_NDEF);
}

#[test]
fn the_uri_and_text_encodings_follow_the_nfc_forum_rtds() {
    let uri = ndef::encode_message(&[NdefRecord::Uri("https://example.com/p".to_string())]);
    assert_eq!(
        uri,
        vec![
            0xD1, 0x01, 0x0E, b'U', 0x04, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c',
            b'o', b'm', b'/', b'p',
        ]
    );
    assert_eq!(ndef::tlv_header(uri.len()), vec![ndef::TLV_NDEF, 0x12]);

    let text = ndef::encode_message(&[NdefRecord::Text {
        lang: "en".to_string(),
        text: "hi".to_string(),
    }]);
    assert_eq!(
        text,
        vec![0xD1, 0x01, 0x05, b'T', 0x02, b'e', b'n', b'h', b'i']
    );
    assert_eq!(ndef::tlv_header(text.len()), vec![ndef::TLV_NDEF, 0x09]);

    // The prefix table compresses the longest match it knows and leaves the rest alone.
    assert_eq!(ndef::encode_uri("https://www.a.b"), (2, "a.b"));
    assert_eq!(ndef::encode_uri("ftp://a.b"), (0, "ftp://a.b"));
    assert_eq!(ndef::decode_uri(5, "+1234"), "tel:+1234".to_string());
}

/// A MIME record, the basis of the Wi-Fi Simple Configuration record, carries type and payload
/// untouched; 255 bytes or more switches the TLV length to its three-byte form.
#[test]
fn a_mime_record_round_trips_and_a_long_message_uses_the_ff_length() {
    let record = NdefRecord::Mime {
        mime_type: "application/vnd.wfa.wsc".to_string(),
        payload: vec![0x10, 0x4A, 0x00, 0x01, 0x10],
    };
    let bytes = ndef::encode_message(std::slice::from_ref(&record));
    assert_eq!(bytes[0], 0xD2, "MB|ME|SR|TNF=2");
    assert_eq!(bytes[1], 23, "type length");
    assert_eq!(bytes[2], 5, "payload length");
    assert_eq!(ndef::decode_message(&bytes), Ok(vec![record]));

    assert_eq!(ndef::tlv_header(0xFE), vec![ndef::TLV_NDEF, 0xFE]);
    assert_eq!(
        ndef::tlv_header(0x0123),
        vec![ndef::TLV_NDEF, 0xFF, 0x01, 0x23]
    );
}

/// User memory is 144 bytes.
#[test]
fn a_message_that_does_not_fit_is_refused() {
    let mut card = Ntag213::new(&mut DetRng::new(5));
    let long = NdefRecord::Mime {
        mime_type: "x".to_string(),
        payload: vec![0u8; 200],
    };
    match card.ndef_write(&[long]) {
        Err(CardError::NdefTooLong { need, have }) => {
            assert!(need > have);
            assert_eq!(have, 144 - 5, "from the NDEF TLV to the end of user memory");
        }
        other => panic!("a 200-byte payload should not fit: {other:?}"),
    }
    assert_eq!(card.ndef_read(), Ok(vec![]), "the tag is unchanged");
}

#[test]
fn a_tap_runs_its_ops_against_the_card() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));
    board
        .world
        .card
        .command(&[cmd::WRITE, PAGE_CFG1, 0x10, 0x00, 0x00, 0x00]);

    let effect = board.apply_input(
        VTime::from_ms(1),
        &InputEvent::NfcTap {
            ops: vec![
                NfcOp::FieldOn,
                NfcOp::Cmd(vec![cmd::GET_VERSION]),
                NfcOp::Cmd(vec![cmd::READ, 0x04]),
                NfcOp::Cmd(vec![cmd::WRITE, PAGE_PWD, 1, 2, 3, 4]),
                NfcOp::FieldOff,
            ],
        },
        &mut cx,
    );
    let tap = effect.card.expect("a tap reports its result");
    assert_eq!(tap.responses.len(), 3);
    assert_eq!(tap.responses[0], VERSION.to_vec());
    assert_eq!(tap.responses[1].len(), 16);
    assert_eq!(tap.responses[2], vec![ACK]);
    assert_eq!(tap.uid, board.world.card.uid());
    assert_eq!(tap.counter, 1);
    assert!(!board.world.card.in_field(), "the tap left the field");
}
