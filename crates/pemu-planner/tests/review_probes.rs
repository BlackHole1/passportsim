//! Regression tests from the planner's safety review probes.

mod support;

use pemu_loader::partitions::subtype;

use pemu_planner::plan::{
    ImageSource, InputSegment, Origin, PlanRequest, encode_partition_table, plan_flash,
};
use pemu_planner::rules::Rule;
use support::*;

/// A second partition table at 0x8000 would be the one written last.
#[test]
fn a_duplicate_table_segment_moving_cardid_is_refused() {
    let good = encode_partition_table(&official_layout());
    let mut moved = official_layout();
    moved[3].offset = 0x40_0000;
    let bad = encode_partition_table(&moved);
    let boot = bootloader();
    let app = app_image(ELF_SHA, 0x1000);
    let segs = [
        InputSegment {
            name: "boot",
            offset: 0,
            data: &boot,
        },
        InputSegment {
            name: "pt-good",
            offset: 0x8000,
            data: &good,
        },
        InputSegment {
            name: "pt-bad",
            offset: 0x8000,
            data: &bad,
        },
        InputSegment {
            name: "app",
            offset: 0x1_0000,
            data: &app,
        },
    ];
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
        Some(&facts()),
    );
    assert!(out.accepted().is_none());
    assert!(out.refused_by(Rule::SegmentOverlap), "{:?}", out.refusals);

    // Two bootloaders, and an app overlapping the table's sector, are refused the same way.
    let segs = [
        InputSegment {
            name: "boot1",
            offset: 0,
            data: &boot,
        },
        InputSegment {
            name: "boot2",
            offset: 0,
            data: &boot,
        },
        InputSegment {
            name: "pt",
            offset: 0x8000,
            data: &good,
        },
        InputSegment {
            name: "app",
            offset: 0x1_0000,
            data: &app,
        },
    ];
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
        Some(&facts()),
    );
    assert!(out.refused_by(Rule::SegmentOverlap), "{:?}", out.refusals);

    // No table segment at all is refused too.
    let segs = [InputSegment {
        name: "app",
        offset: 0x1_0000,
        data: &app,
    }];
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
        Some(&facts()),
    );
    assert!(out.refused_by(Rule::SegmentOverlap), "{:?}", out.refusals);
}

fn nvs_as_app_layout() -> Vec<pemu_loader::partitions::Partition> {
    use pemu_loader::partitions::{ptype, subtype};
    let mut layout = vec![part("ota_0", ptype::APP, subtype::OTA_0, 0x9000, 0x7000)];
    layout.extend(
        official_layout()
            .into_iter()
            .filter(|p| p.name == "factory" || p.name == "cardid"),
    );
    layout
}

#[test]
fn a_table_repurposing_nvs_as_an_app_is_refused_in_both_forms() {
    let layout = nvs_as_app_layout();
    let app = app_image(ELF_SHA, 0x1000);
    let mut img = merged(&layout, &app, 0x2_0000);
    let small = app_image(ELF_SHA, 0x100);
    img[0x9000..0x9000 + small.len()].copy_from_slice(&small);
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Merged(&img), Origin::Tool),
        Some(&facts()),
    );
    assert!(out.accepted().is_none());
    assert!(out.refused_by(Rule::WritesDeviceData), "{:?}", out.refusals);
    assert!(
        out.refused_by(Rule::DataLayoutMismatch),
        "{:?}",
        out.refusals
    );

    let table = encode_partition_table(&layout);
    let segs = [
        InputSegment {
            name: "pt",
            offset: 0x8000,
            data: &table,
        },
        InputSegment {
            name: "ota",
            offset: 0x9000,
            data: &small,
        },
    ];
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
        Some(&facts()),
    );
    assert!(out.refused_by(Rule::WritesDeviceData), "{:?}", out.refusals);
    assert!(
        out.refused_by(Rule::DataLayoutMismatch),
        "{:?}",
        out.refusals
    );
}

/// `--erase-nvs` erases contents only.
#[test]
fn no_one_may_change_the_nvs_entry() {
    let mut layout = official_layout();
    layout[0].size = 0x5000;
    let img = merged(&layout, &app_image(ELF_SHA, 0x1000), 0x2_0000);
    let mut request = PlanRequest::write(ImageSource::Merged(&img), Origin::Tool);
    let out = plan_flash(&request, Some(&facts()));
    assert!(
        out.refused_by(Rule::DataLayoutMismatch),
        "{:?}",
        out.refusals
    );
    request.origin = Origin::HumanCli;
    request.erase_nvs = true;
    let out = plan_flash(&request, Some(&facts()));
    assert!(
        out.refused_by(Rule::DataLayoutMismatch),
        "{:?}",
        out.refusals
    );
    layout[0].size = 0x6000;
    layout[1].offset = 0xE000;
    let img = merged(&layout, &app_image(ELF_SHA, 0x1000), 0x2_0000);
    request.image = ImageSource::Merged(&img);
    let out = plan_flash(&request, Some(&facts()));
    assert!(
        out.refused_by(Rule::DataLayoutMismatch),
        "phy_init moved: {:?}",
        out.refusals
    );
}

#[test]
fn erase_nvs_never_follows_a_moved_nvs() {
    let app = app_image(ELF_SHA, 0x1000);
    let mut moved = official_layout();
    moved[0].offset = 0x31_0000;
    moved[0].size = 0x4_6000;
    let img = merged(&moved, &app, 0x1_0000 + app.len());
    for origin in [Origin::HumanCli, Origin::Tool] {
        let mut request = PlanRequest::write(ImageSource::Merged(&img), origin);
        request.erase_nvs = true;
        let out = plan_flash(&request, Some(&facts()));
        assert!(out.accepted().is_none(), "{origin:?}");
        assert!(
            out.refused_by(Rule::DataLayoutMismatch),
            "{:?}",
            out.refusals
        );
        assert_ne!(out.plan.erase_nvs, Some((0x31_0000, 0x4_6000)));
    }
    let mut retyped = official_layout();
    retyped[0].subtype = subtype::PHY;
    let img = merged(&retyped, &app, 0x1_0000 + app.len());
    let mut request = PlanRequest::write(ImageSource::Merged(&img), Origin::HumanCli);
    request.erase_nvs = true;
    let out = plan_flash(&request, Some(&facts()));
    assert!(
        out.refused_by(Rule::DataLayoutMismatch),
        "{:?}",
        out.refusals
    );

    let img = official_unpadded();
    let mut request = PlanRequest::write(ImageSource::Merged(&img), Origin::HumanCli);
    request.erase_nvs = true;
    let out = plan_flash(&request, Some(&facts()));
    assert!(out.accepted().is_some(), "{:?}", out.refusals);
    let nvs = facts()
        .partitions
        .into_iter()
        .find(|p| p.name == "nvs")
        .expect("nvs");
    assert_eq!(out.plan.erase_nvs, Some((nvs.offset, nvs.size)));
}

#[test]
fn every_non_app_device_partition_is_protected() {
    use pemu_loader::partitions::ptype;
    let mut device = facts();
    device
        .partitions
        .push(part("storage", ptype::DATA, 0x82, 0x40_0000, 0x10_0000));
    device
        .partitions
        .push(part("custom", 0x40, 0x01, 0x60_0000, 0x10_0000));
    let app = app_image(ELF_SHA, 0x100);
    for (over, offset) in [("custom", 0x60_0000), ("storage", 0x40_0000)] {
        let mut layout = official_layout();
        layout.extend(
            device
                .partitions
                .iter()
                .filter(|p| p.name != over && !official_layout().iter().any(|o| o.name == p.name))
                .cloned(),
        );
        layout.push(part("ota_0", ptype::APP, subtype::OTA_0, offset, 0x10_0000));
        let table = encode_partition_table(&layout);
        let segs = [
            InputSegment {
                name: "pt",
                offset: 0x8000,
                data: &table,
            },
            InputSegment {
                name: "ota",
                offset,
                data: &app,
            },
        ];
        let out = plan_flash(
            &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
            Some(&device),
        );
        assert!(out.accepted().is_none(), "{over}");
        assert!(
            out.refused_by(Rule::WritesDeviceData),
            "{over}: {:?}",
            out.refusals
        );
        assert!(
            out.refused_by(Rule::DataLayoutMismatch),
            "{over}: {:?}",
            out.refusals
        );
    }
    // Keeping both partitions and writing elsewhere is fine.
    let mut layout = official_layout();
    layout.extend(
        device
            .partitions
            .iter()
            .filter(|p| p.name == "storage" || p.name == "custom")
            .cloned(),
    );
    layout.push(part(
        "ota_0",
        ptype::APP,
        subtype::OTA_0,
        0x70_0000,
        0x10_0000,
    ));
    let table = encode_partition_table(&layout);
    let segs = [
        InputSegment {
            name: "pt",
            offset: 0x8000,
            data: &table,
        },
        InputSegment {
            name: "ota",
            offset: 0x70_0000,
            data: &app,
        },
    ];
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
        Some(&device),
    );
    assert!(out.accepted().is_some(), "{:?}", out.refusals);
}

/// Two apps that overlap only once rounded to sectors, a bootloader running into the table, and
/// the `flash_id` warning line.
#[test]
fn the_second_probe_set_stays_refused() {
    use pemu_loader::partitions::ptype;
    let mut layout = official_layout();
    layout.push(part("ota_0", ptype::APP, subtype::OTA_0, 0x32_0000, 0x800));
    layout.push(part(
        "ota_1",
        ptype::APP,
        subtype::OTA_0 + 1,
        0x32_0800,
        0x800,
    ));
    let table = encode_partition_table(&layout);
    let a = app_image(ELF_SHA, 0x10);
    let segs = [
        InputSegment {
            name: "pt",
            offset: 0x8000,
            data: &table,
        },
        InputSegment {
            name: "a",
            offset: 0x32_0000,
            data: &a,
        },
        InputSegment {
            name: "b",
            offset: 0x32_0800,
            data: &a,
        },
    ];
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
        Some(&facts()),
    );
    assert!(out.refused_by(Rule::SegmentOverlap), "{:?}", out.refusals);

    let boot = vec![0xE9u8; 0x8001];
    let table = encode_partition_table(&official_layout());
    let segs = [
        InputSegment {
            name: "boot",
            offset: 0,
            data: &boot,
        },
        InputSegment {
            name: "pt",
            offset: 0x8000,
            data: &table,
        },
    ];
    let out = plan_flash(
        &PlanRequest::write(ImageSource::Segments(&segs), Origin::Tool),
        Some(&facts()),
    );
    assert!(out.accepted().is_none());
    assert!(out.refused_by(Rule::SegmentOverlap), "{:?}", out.refusals);

    let warning = "Device PID identification is only supported on COM and /dev/ serial ports.\nManufacturer: 20\nDevice: 4017\n";
    assert_eq!(
        pemu_planner::rehearse::parse_flash_id(warning),
        Some((0x20, 0x4017))
    );
}
