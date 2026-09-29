//! Fixtures for the provenance rules, including the false positives the check must not raise.

use super::*;

/// A model-file fixture.
fn file(path: &str, text: &str) -> SourceFile {
    SourceFile {
        path: path.to_string(),
        text: text.to_string(),
    }
}

fn input_models(models: Vec<SourceFile>) -> Input {
    Input {
        models,
        ..Input::default()
    }
}

#[test]
fn model_header_without_a_citation_fails() {
    let models = vec![file(
        "crates/pemu-board/src/st7789.rs",
        "//! The panel model.\n\npub struct St7789p3 {}\n",
    )];
    let report = check(&input_models(models));
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.findings[0].rule, Rule::ModelNoCitation);
    assert_eq!(report.findings[0].line, 1);
}

#[test]
fn model_headers_citing_a_permitted_source_pass() {
    let headers = [
        "//! Panel reset sequence from `specs/blocks/spi2.toml` rows.\n",
        "//! Busy-wait row from IDF hal/esp32c3/include/hal/spi_ll.h:264-268.\n",
        "//! Layout of the TRM section 3.2 and the datasheet.\n",
        "//! Settled on the device by probes/probe_reset.\n",
        "//! The regi2c store model; its reset values are UNVERIFIED.\n",
    ];
    for header in headers {
        let models = vec![file("crates/pemu-hle/src/worker.rs", header)];
        let report = check(&input_models(models));
        assert_eq!(report.findings, Vec::new(), "{header}");
    }
}

#[test]
fn store_only_stubs_are_not_model_files() {
    let stub = "//! A store-only stub.\n\n\
                pub type Model = StoreOnly<super::block::Gpio>;\n";
    assert!(!models::is_model(
        "crates/pemu-soc-c3/src/periph/gpio.rs",
        stub
    ));
    let model = "//! The GPIO model (`specs/blocks/gpio.toml`).\n\npub struct Model {}\n";
    assert!(models::is_model(
        "crates/pemu-soc-c3/src/periph/gpio.rs",
        model
    ));
    assert!(!models::is_model(
        "crates/pemu-soc-c3/src/periph/store_only.rs",
        "//! The store-only register file.\npub struct StoreOnly<B> {}\n"
    ));
}

#[test]
fn mentioning_store_only_is_not_a_stub_declaration() {
    // Only the file's own `pub type ... = StoreOnly<...>;` alias marks a stub. A comment naming
    // the generic, or the `c3_devices!` table of `periph/mod.rs` listing store-only blocks, is
    // model data and stays in the model set.
    let table = "//! The device table (`specs/blocks/`).\n\n\
                 /// A generic model such as `StoreOnly<B>` takes its constants from the table.\n\
                 c3_devices! {\n    uart1: Uart1, UART1 @ 0x6001_0000, 0x1000 => \
                 StoreOnly<block::Uart1>;\n}\n";
    assert!(models::is_model(
        "crates/pemu-soc-c3/src/periph/mod.rs",
        table
    ));
    let alias = "//! TIMG0 (`specs/blocks/timg0.toml`).\n\n\
                 pub type Timg0Model = StoreOnly<super::block::Timg0>;\n";
    assert!(!models::is_model(
        "crates/pemu-soc-c3/src/periph/timg.rs",
        alias
    ));
}

#[test]
fn model_scopes_cover_the_whole_soc_crate() {
    // The address space, MMU, interrupt fabric and peripheral wiring of `crates/pemu-soc-c3/src`
    // are models too, not only `periph/`.
    assert!(
        models::SCOPES.contains(&"crates/pemu-soc-c3/src"),
        "{:?}",
        models::SCOPES
    );
    assert!(
        !models::SCOPES
            .iter()
            .any(|scope| scope.starts_with("crates/pemu-soc-c3/src/")),
        "a subdirectory scope would collect its files twice: {:?}",
        models::SCOPES
    );
}

#[test]
fn notes_path_patterns_fail_and_prose_passes() {
    let bad = "# G3 behavior\n\nSeen in src/idle.rs:42 of hw/esp32c3 and esp32sim-0.4/core.\n";
    let input = Input {
        notes: vec![file("specs/notes/g3-behavior.md", bad)],
        ..Input::default()
    };
    let report = check(&input);
    assert_eq!(report.findings.len(), 3, "{:?}", report.findings);
    assert!(report.findings.iter().all(|f| f.rule == Rule::NotesPath));
    assert!(report.findings.iter().all(|f| f.line == 3));

    let good = "# G3 behavior\n\nAn esp32sim run stalls at the same point; the idle chunk size \
                does not change guest state.\n";
    let input = Input {
        notes: vec![file("specs/notes/g3-behavior.md", good)],
        ..Input::default()
    };
    assert_eq!(check(&input).findings, Vec::new());
}

#[test]
fn block_spec_row_without_provenance_fails() {
    let text = "schema = 1\nblock = \"spi2\"\nclass = \"B\"\nmilestone = \"M4\"\n\
                provenance = \"TRM SPI2 chapter\"\n\n\
                [[wait]]\nid = \"spi2.cmd_update\"\nregister = \"SPI_CMD\"\n\n\
                [[stable_read]]\nregister = \"SPI_CMD\"\nprovenance = \"TRM SPI2 chapter\"\n";
    let input = Input {
        blocks: vec![file("specs/blocks/spi2.toml", text)],
        ..Input::default()
    };
    let report = check(&input);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.findings[0].rule, Rule::SpecProvenance);
    assert_eq!(report.findings[0].line, 7, "the `[[wait]]` header line");
    assert!(
        report.findings[0].detail.contains("spi2.cmd_update"),
        "{}",
        report.findings[0]
    );
}

#[test]
fn block_spec_header_without_provenance_fails() {
    let text = "# Block spec.\nschema = 1\nblock = \"aes\"\nclass = \"U\"\nmilestone = \"M8\"\n";
    let input = Input {
        blocks: vec![file("specs/blocks/aes.toml", text)],
        ..Input::default()
    };
    let report = check(&input);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.findings[0].rule, Rule::SpecProvenance);
    assert_eq!(report.findings[0].line, 2, "the first header key");
}

#[test]
fn block_spec_provenance_citing_a_restricted_source_fails() {
    // A `provenance` value never names a path of a GPL or unlicensed source.
    let text = "schema = 1\nblock = \"spi2\"\n\
                provenance = \"TRM SPI2 chapter; esp32sim/src/spi.rs\"\n\n\
                [[wait]]\nid = \"spi2.cmd_update\"\n\
                provenance = \"hw/misc/esp32c3_sha.c:88\"\n\n\
                [[stable_read]]\nregister = \"SPI_CMD\"\n\
                provenance = \"IDF hal/esp32c3/include/hal/spi_ll.h:264-268\"\n";
    let input = Input {
        blocks: vec![file("specs/blocks/spi2.toml", text)],
        ..Input::default()
    };
    let report = check(&input);
    let shown: Vec<String> = report.findings.iter().map(Finding::to_string).collect();
    assert_eq!(report.findings.len(), 2, "{shown:?}");
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.rule == Rule::SpecProvenance)
    );
    assert!(
        shown
            .iter()
            .any(|f| f.contains("spi2.toml:3:") && f.contains("`esp32sim/`")),
        "{shown:?}"
    );
    assert!(
        shown
            .iter()
            .any(|f| f.contains("spi2.toml:7:") && f.contains("`hw/`")),
        "{shown:?}"
    );
}

#[test]
fn block_spec_with_provenance_everywhere_passes() {
    let text = "schema = 1\nprovenance = \"TRM GPIO chapter\"\n\n\
                [[reset_domains]]\nregisters = \"*\"\nscopes = []\nprovenance = \"IDF soc/reset_reasons.h\"\n\n\
                [[overrides]]\nregister = \"GPIO_STRAP\"\nvalue = 0x0A\n\
                provenance = \"device banner boot:0xa\"\n";
    let input = Input {
        blocks: vec![file("specs/blocks/gpio.toml", text)],
        ..Input::default()
    };
    assert_eq!(check(&input).findings, Vec::new());
}

#[test]
fn invalid_block_spec_is_one_finding() {
    let input = Input {
        blocks: vec![file("specs/blocks/gpio.toml", "schema = \n")],
        ..Input::default()
    };
    let report = check(&input);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert!(report.findings[0].detail.starts_with("invalid TOML"));
}

/// The repository itself passes every rule.
#[test]
fn real_tree_passes_the_rules() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent directory")
        .to_path_buf();
    let input = load(&root).expect("the repository loads");
    let report = check(&input);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.blocks > 40, "block specs: {}", report.blocks);

    // The model set holds the SoC files outside `periph/` and `gen/` and the device table, and
    // still leaves the store-only stubs out.
    let paths: Vec<&str> = input.models.iter().map(|f| f.path.as_str()).collect();
    for path in [
        "crates/pemu-soc-c3/src/mmio.rs",
        "crates/pemu-soc-c3/src/pagetable.rs",
        "crates/pemu-soc-c3/src/wiring/gpio.rs",
        "crates/pemu-soc-c3/src/periph/mod.rs",
        // The MMU and GPIO blocks have real tables, not store-only aliases, so they are model
        // files and carry citations.
        "crates/pemu-soc-c3/src/mmu.rs",
        "crates/pemu-soc-c3/src/periph/extmem.rs",
        "crates/pemu-soc-c3/src/periph/gpio.rs",
    ] {
        assert!(paths.contains(&path), "{path} is a model file");
    }
    // The one file that is the store-only model rather than a block using it.
    let store_only = "crates/pemu-soc-c3/src/periph/store_only.rs";
    assert!(
        !paths.contains(&store_only),
        "{store_only} is still a store-only stub"
    );
}
