//! One test per refusal rule, against the committed device facts. No port, no file.

mod support;

use pemu_loader::partitions::{PartitionTable, ptype, subtype};
use pemu_planner::plan::{
    ImageSource, InputSegment, Origin, PlanRequest, dry_run, encode_partition_table, plan_flash,
};
use pemu_planner::rules::{
    BackupEvidence, CARDID_END, CARDID_OFFSET, ChipRevision, DeviceFacts, Rule, check_backup,
    check_write_range,
};
use support::*;

fn good_backup() -> BackupEvidence {
    BackupEvidence {
        verified: true,
        owner_only: true,
        inside_repository: false,
    }
}

fn tool_plan(image: &[u8], facts: Option<&DeviceFacts>) -> pemu_planner::plan::PlanOutcome {
    plan_flash(
        &PlanRequest::write(ImageSource::Merged(image), Origin::Tool),
        facts,
    )
}

fn assert_fires(outcome: &pemu_planner::plan::PlanOutcome, rule: Rule) {
    assert!(
        outcome.refused_by(rule),
        "expected {} to fire; refusals: {:?}",
        rule.id(),
        outcome.refusals
    );
    assert!(outcome.accepted().is_none());
}

#[test]
fn the_fixture_reads_as_the_passport_facts() {
    let facts = facts();
    assert_eq!(facts, DeviceFacts::passport(official_layout()));
}

#[test]
fn the_fixture_rejects_a_missing_key_instead_of_defaulting() {
    let without_revision = FACTS_TOML.replace("revision = \"v1.1\"", "");
    assert!(DeviceFacts::from_toml(&without_revision).is_err());
    let without_partitions: String = FACTS_TOML
        .lines()
        .take_while(|l| !l.starts_with("[[partition]]"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(DeviceFacts::from_toml(&without_partitions).is_err());
}

#[test]
fn the_encoded_table_carries_an_md5_row_the_loader_accepts() {
    let bytes = encode_partition_table(&official_layout());
    let table = PartitionTable::parse(&bytes).expect("the loader's MD5 check passes");
    assert!(table.has_md5);
    assert_eq!(table.entries, official_layout());
}

#[test]
fn unmodified_facts_and_image_give_an_accepted_plan() {
    let facts = facts();
    for image in [official_unpadded(), official_padded()] {
        let request = PlanRequest::write(ImageSource::Merged(&image), Origin::Tool);
        let outcome = dry_run(&request, &facts, Some(&good_backup()));
        let plan = outcome
            .accepted()
            .unwrap_or_else(|| panic!("refused: {:?}", outcome.refusals));
        let names: Vec<(&str, u32)> = plan
            .writes
            .iter()
            .map(|w| (w.name.as_str(), w.offset))
            .collect();
        assert_eq!(
            names,
            [
                ("bootloader", 0x0),
                ("partition-table", 0x8000),
                ("factory", 0x1_0000)
            ]
        );
        for w in &plan.writes {
            let (s, e) = w.sectors();
            assert!(
                e <= 0x9000 || s >= 0x1_0000,
                "{} touches nvs or phy_init",
                w.name
            );
            assert!(e <= u64::from(CARDID_OFFSET) || s >= u64::from(CARDID_END));
            assert_ne!(w.data.last(), Some(&0xFF), "{} is trimmed", w.name);
        }
        assert_eq!(plan.erase_nvs, None);
        assert_eq!(plan.app_elf_sha256, Some(ELF_SHA));
    }
}

#[test]
fn the_padded_and_unpadded_images_give_the_same_plan_digest() {
    let a = tool_plan(&official_unpadded(), None);
    let b = tool_plan(&official_padded(), None);
    assert_eq!(a.plan.plan_sha256, b.plan.plan_sha256);
    assert_eq!(a.plan.plan_sha256_hex().len(), 64);
}

#[test]
fn rule_chip_revision() {
    let mut facts = facts();
    facts.revision = ChipRevision { major: 0, minor: 4 };
    assert_fires(
        &tool_plan(&official_unpadded(), Some(&facts)),
        Rule::ChipRevision,
    );
}

#[test]
fn rule_flash_id() {
    let mut facts = facts();
    facts.flash_manufacturer = 0xC8;
    assert_fires(
        &tool_plan(&official_unpadded(), Some(&facts)),
        Rule::FlashId,
    );
    let mut facts = support::facts();
    facts.flash_device = 0x4016;
    assert_fires(
        &tool_plan(&official_unpadded(), Some(&facts)),
        Rule::FlashId,
    );
}

#[test]
fn rule_cardid_missing() {
    let mut facts = facts();
    facts.partitions.retain(|p| p.name != "cardid");
    assert_fires(
        &tool_plan(&official_unpadded(), Some(&facts)),
        Rule::CardidMissing,
    );
}

#[test]
fn rule_cardid_moved() {
    for (offset, size) in [(0x35_7000, 0x4000), (0x35_6000, 0x3000)] {
        let mut facts = facts();
        let cardid = facts.partitions.iter_mut().find(|p| p.name == "cardid");
        let cardid = cardid.expect("fixture has cardid");
        cardid.offset = offset;
        cardid.size = size;
        assert_fires(
            &tool_plan(&official_unpadded(), Some(&facts)),
            Rule::CardidMoved,
        );
    }
}

#[test]
fn rule_image_moves_cardid() {
    let mut layout = official_layout();
    layout[3].size = 0x2000;
    let image = merged(&layout, &app_image(ELF_SHA, 0), 0x2_0000);
    assert_fires(&tool_plan(&image, Some(&facts())), Rule::ImageMovesCardid);
    layout.pop();
    let image = merged(&layout, &app_image(ELF_SHA, 0), 0x2_0000);
    assert_fires(&tool_plan(&image, None), Rule::ImageMovesCardid);
}

#[test]
fn rule_cardid_overlap() {
    // An app slot that ends where cardid starts; an app one byte longer reaches into the window.
    let mut layout = official_layout();
    layout.push(part("ota_0", ptype::APP, subtype::OTA_0, 0x35_0000, 0x6000));
    let table = encode_partition_table(&layout);
    let boot = bootloader();
    let app = app_image(ELF_SHA, 0x6001 - 304);
    let segments = [
        InputSegment {
            name: "bootloader.bin",
            offset: 0,
            data: &boot,
        },
        InputSegment {
            name: "partition-table.bin",
            offset: 0x8000,
            data: &table,
        },
        InputSegment {
            name: "ota.bin",
            offset: 0x35_0000,
            data: &app,
        },
    ];
    let request = PlanRequest::write(ImageSource::Segments(&segments), Origin::Tool);
    assert_fires(&plan_flash(&request, Some(&facts())), Rule::CardidOverlap);
    // The rounding itself: a range ending one byte into the window's sector, and one starting in
    // the last sector of the window.
    assert!(
        !check_write_range("x", 0x35_5000, 0x1000)
            .iter()
            .any(|r| r.rule == Rule::CardidOverlap)
    );
    assert!(
        check_write_range("x", 0x35_5800, 0x801)
            .iter()
            .any(|r| r.rule == Rule::CardidOverlap)
    );
    assert!(
        check_write_range("x", 0x35_9FFF, 1)
            .iter()
            .any(|r| r.rule == Rule::CardidOverlap)
    );
    assert!(
        !check_write_range("x", 0x35_A000, 0x10)
            .iter()
            .any(|r| r.rule == Rule::CardidOverlap)
    );
}

#[test]
fn rule_beyond_flash() {
    let mut image = official_padded();
    image.push(0xFF);
    assert_fires(&tool_plan(&image, None), Rule::BeyondFlash);
    assert!(
        check_write_range("x", 0x7F_F000, 0x1001)
            .iter()
            .any(|r| r.rule == Rule::BeyondFlash)
    );
}

#[test]
fn rule_app_too_large() {
    let big = app_image(ELF_SHA, 0x30_0001 - 304);
    let mut layout = official_layout();
    layout[2].size = 0x34_0000;
    let image = merged(&layout, &big, 0x40_0000);
    assert_fires(&tool_plan(&image, None), Rule::AppTooLarge);

    let table = encode_partition_table(&official_layout());
    let segments = [
        InputSegment {
            name: "partition-table.bin",
            offset: 0x8000,
            data: &table,
        },
        InputSegment {
            name: "app.bin",
            offset: 0x1_0000,
            data: &big,
        },
    ];
    let request = PlanRequest::write(ImageSource::Segments(&segments), Origin::Tool);
    assert_fires(&plan_flash(&request, Some(&facts())), Rule::AppTooLarge);

    let exact = app_image(ELF_SHA, 0x30_0000 - 304);
    let image = merged(&official_layout(), &exact, 0x80_0000);
    assert!(
        tool_plan(&image, None).accepted().is_some(),
        "exactly 0x300000 is allowed"
    );
}

#[test]
fn rule_data_dropped() {
    for (at, name) in [
        (0x9000, "nvs"),
        (0xF800, "phy_init"),
        (0x35_7000, "cardid"),
        (0x40_0000, ""),
    ] {
        let mut image = official_padded();
        image[at] = 0x00;
        let outcome = tool_plan(&image, None);
        assert_fires(&outcome, Rule::DataDropped);
        let detail = &outcome
            .refusals
            .iter()
            .find(|r| r.rule == Rule::DataDropped)
            .expect("fired")
            .detail;
        assert!(detail.contains(name), "{detail}");
    }
}

#[test]
fn rule_segment_not_allowed() {
    let data = vec![0u8; 0x100];
    let segments = [InputSegment {
        name: "nvs.bin",
        offset: 0x9000,
        data: &data,
    }];
    let request = PlanRequest::write(ImageSource::Segments(&segments), Origin::Tool);
    assert_fires(
        &plan_flash(&request, Some(&facts())),
        Rule::SegmentNotAllowed,
    );
}

#[test]
fn rule_erase_requested() {
    let image = official_unpadded();
    let mut request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
    request.erase_all = true;
    assert_fires(&plan_flash(&request, None), Rule::EraseRequested);
    let mut request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
    request.erase_regions = vec![(0x35_8000, 0x1000)];
    let outcome = plan_flash(&request, None);
    assert_fires(&outcome, Rule::EraseRequested);
    assert!(outcome.refusals[0].detail.contains("covers cardid"));
}

#[test]
fn rule_erase_nvs_not_human() {
    let image = official_unpadded();
    let mut request = PlanRequest::write(ImageSource::Merged(&image), Origin::Tool);
    request.erase_nvs = true;
    assert_fires(&plan_flash(&request, None), Rule::EraseNvsNotHuman);
    request.origin = Origin::HumanCli;
    let outcome = plan_flash(&request, None);
    let plan = outcome.accepted().expect("a person may erase nvs");
    assert_eq!(plan.erase_nvs, Some((0x9000, 0x6000)));
    assert_ne!(
        plan.plan_sha256,
        tool_plan(&image, None).plan.plan_sha256,
        "the erase is bound into the plan digest"
    );
}

#[test]
fn rule_verify_firmware() {
    let mut image = official_unpadded();
    // Drop the MD5 row: turn it into the end-of-table row.
    let row = 0x8000 + 4 * 32;
    image[row..row + 32].fill(0xFF);
    assert_fires(&tool_plan(&image, None), Rule::VerifyFirmware);
    let mut image = official_unpadded();
    image[0x1_0000] = 0xEA;
    assert_fires(&tool_plan(&image, None), Rule::VerifyFirmware);
}

#[test]
fn rule_no_verified_backup() {
    let image = official_unpadded();
    let request = PlanRequest::write(ImageSource::Merged(&image), Origin::HumanCli);
    assert_fires(&dry_run(&request, &facts(), None), Rule::NoVerifiedBackup);
    let unverified = BackupEvidence {
        verified: false,
        ..good_backup()
    };
    assert_fires(
        &dry_run(&request, &facts(), Some(&unverified)),
        Rule::NoVerifiedBackup,
    );
}

#[test]
fn rule_backup_not_owner_only() {
    let wide = BackupEvidence {
        owner_only: false,
        ..good_backup()
    };
    let fired = check_backup(Some(&wide));
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].rule, Rule::BackupNotOwnerOnly);
}

#[test]
fn rule_backup_inside_repository() {
    let inside = BackupEvidence {
        inside_repository: true,
        ..good_backup()
    };
    let fired = check_backup(Some(&inside));
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].rule, Rule::BackupInsideRepository);
}

#[test]
fn every_refusal_is_collected_not_only_the_first() {
    let mut facts = facts();
    facts.revision = ChipRevision { major: 0, minor: 3 };
    facts.flash_manufacturer = 0xC8;
    let mut image = official_padded();
    image[0x9000] = 0;
    let mut request = PlanRequest::write(ImageSource::Merged(&image), Origin::Tool);
    request.erase_all = true;
    let outcome = dry_run(&request, &facts, None);
    for rule in [
        Rule::ChipRevision,
        Rule::FlashId,
        Rule::DataDropped,
        Rule::EraseRequested,
        Rule::NoVerifiedBackup,
    ] {
        assert!(outcome.refused_by(rule), "{}", rule.id());
    }
}

#[test]
fn rule_ids_are_unique_and_snake_case() {
    let mut ids: Vec<&str> = Rule::ALL.iter().map(|r| r.id()).collect();
    assert!(
        ids.iter()
            .all(|id| id.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    );
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), Rule::ALL.len());
}

/// Seen with `probe_campaign_timing`: the write is trimmed of trailing 0xFF, and the descriptor
/// must still be read, or the boot check refuses a good flash.
#[test]
fn an_app_image_ending_in_0xff_keeps_its_elf_sha256() {
    let mut app = app_image(ELF_SHA, 0);
    let last = app.len() - 1;
    app[last] = 0xFF;
    let image = merged(&official_layout(), &app, 0x1_0000 + app.len());
    let request = PlanRequest::write(ImageSource::Merged(&image), Origin::Tool);
    let outcome = dry_run(&request, &facts(), Some(&good_backup()));
    let plan = outcome
        .accepted()
        .unwrap_or_else(|| panic!("refused: {:?}", outcome.refusals));
    let factory = plan
        .writes
        .iter()
        .find(|w| w.name == "factory")
        .expect("an app write");
    assert_eq!(
        factory.data.len(),
        app.len() - 1,
        "the trailing 0xFF is trimmed"
    );
    assert_eq!(plan.app_elf_sha256, Some(ELF_SHA));
}
