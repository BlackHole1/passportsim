//! What a `var:` page watch costs on a boot. Marking a page `PF_SLOW` takes every store to it,
//! and to every alias of it, off the fast store path of `pemu_rv32`.
//!
//! A measurement, not an assertion: it prints wall times with and without the watch and never
//! fails on a number. The structural property (a run unmarks every page it marked) is asserted in
//! `pemu_machine`.
//!
//! ```text
//! cargo test --release -p pemu-host --test watch_cost -- --ignored --nocapture
//! ```

// Test-only file and env access.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use pemu_core::time::VTime;
use pemu_introspect::vars::VarQuery;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{StopSet, Watch};

/// Long enough to be well into the application, so the stores measured are the guest's.
const BOOT_MS: u64 = 1_500;

/// Repeats per leg; the best is reported, as the one least polluted by other host load.
const REPEATS: usize = 11;

fn corpus_path(id: &str, key: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let manifest = home.join(".config/passportsim/corpus.toml");
    let text = std::fs::read_to_string(&manifest).ok()?;
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == format!("[{id}]");
        } else if in_entry
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            let raw = v.trim().trim_matches('"');
            let path = match raw.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(raw),
            };
            return path.exists().then_some(path);
        }
    }
    None
}

#[test]
#[ignore = "a measurement, not an assertion; run with --ignored --nocapture"]
fn a_var_page_watch_on_an_official_boot() {
    let (Some(bin), Some(elf)) = (
        corpus_path("official", "bin"),
        corpus_path("official", "elf"),
    ) else {
        eprintln!("skip: the official corpus image or ELF is absent");
        return;
    };
    let image = Arc::new(std::fs::read(&bin).expect("the corpus image is readable"));
    let elf_bytes = std::fs::read(&elf).expect("the corpus ELF is readable");
    let context = pemu_host::hooks::ElfContext::parse(&elf_bytes).expect("the ELF parses");

    let globals = context
        .globals
        .get(&context.elf, &context.bytes)
        .expect("the globals resolve");
    let sel = globals
        .resolve(&VarQuery::parse("s_sel").expect("a name"))
        .expect("official declares s_sel");
    let watch = Watch {
        addr: sel.addr,
        len: sel.size().max(1),
    };
    assert!(watch.watchable(), "s_sel is in watchable RAM");

    let armed = StopSet {
        watches: vec![watch],
        ..StopSet::default()
    };
    armed.check().expect("the machine can arm it");

    let build = || {
        let flash = pemu_host::backend::merged_image(&image).expect("the image merges");
        pemu_host::backend::build_machine(flash, 0).expect("the official machine builds")
    };
    // Where the watch fires, measured rather than guessed: the control leg is cut there.
    let cut = {
        let mut machine = build();
        machine.run(RunLimits {
            until: Some(VTime::from_ms(BOOT_MS)),
            max_insns: None,
            stops: armed.clone(),
        });
        machine.now()
    };

    // One timed boot to `BOOT_MS`, resuming at every stop. Both legs must do the same guest work:
    // a watch ends the run at the hitting store, so one `run` call covers less virtual time with
    // it armed. The re-arm at each resume is part of the price.
    let boot = |stops: &StopSet, cuts: &[VTime]| -> (f64, u32) {
        let mut machine = build();
        let mut resumes = 0;
        let at = Instant::now();
        for until in cuts {
            while machine.now().0 < until.0 {
                machine.run(RunLimits {
                    until: Some(*until),
                    max_insns: None,
                    stops: stops.clone(),
                });
                resumes += 1;
                assert!(resumes < 100_000, "the boot made no progress");
            }
        }
        (at.elapsed().as_secs_f64() * 1000.0, resumes)
    };

    // Interleaved, one repeat of each leg per round: run in sequence on a loaded host, the legs
    // drifted by 2.5x and best-of cancels only noise every leg sees equally.
    let whole = [VTime::from_ms(BOOT_MS)];
    let halves = [cut, VTime::from_ms(BOOT_MS)];
    let (mut plain, mut split, mut watched) = (f64::INFINITY, f64::INFINITY, f64::INFINITY);
    let (mut plain_resumes, mut watched_resumes) = (0, 0);
    for _ in 0..REPEATS {
        let (t, n) = boot(&StopSet::default(), &whole);
        plain = plain.min(t);
        plain_resumes = n;
        // No watch, but cut into two `run` calls where it fires: separates "the page is slow"
        // from "the run was split", since a second call re-arms and re-translates.
        let (t, _) = boot(&StopSet::default(), &halves);
        split = split.min(t);
        let (t, n) = boot(&armed, &whole);
        watched = watched.min(t);
        watched_resumes = n;
    }

    let delta = watched - plain;
    println!(
        "MEASURED var-watch on an official boot to {BOOT_MS} ms of virtual time \
         ({REPEATS} repeats, best of each, resumed to the same deadline both ways): \
         no watch {plain:.1} ms in {plain_resumes} run call(s), \
         no watch but split in two at the same instant {split:.1} ms, \
         watch on s_sel ({:#010x}, page {:#010x}) {watched:.1} ms in {watched_resumes} \
         run call(s), delta {delta:+.1} ms ({:+.1}%) of which {:+.1} ms is the extra run call \
         and {:+.1} ms is the slow page",
        watch.addr,
        watch.addr & !0xFFF,
        delta / plain * 100.0,
        split - plain,
        watched - split
    );
}
