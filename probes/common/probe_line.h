// Machine-readable console line format shared by every probe firmware.
// MIT, part of this repository.
//
// A probe prints ordinary IDF log lines like any other app; the facts a harness consumes are
// carried by *probe lines*, which are the only lines a parser looks at:
//
//     TAG|positional|positional|key=value|key=value
//
//   - TAG      one to sixteen characters of [A-Z][A-Z0-9_]*, at column 0.
//   - segments separated by '|'. Zero or more positional segments come first; once a segment
//     contains '=', every following segment must be `key=value`.
//   - key      one to thirty-two characters of [a-z][a-z0-9_]*.
//   - value    printable ASCII (0x20 to 0x7E) without '|'; may be empty.
//   - the line ends with '\n' and carries no trailing '|'.
//
// Anything else on the console (ROM banner, bootloader lines, `I (123) tag: ...`) is not a probe
// line and is skipped. The shape follows the earlier prototype probes, which already print
// `HEAP|stage|free=..|largest=..`, so one parser serves both.
//
// The Rust side of this contract is `xtask/src/probes/line.rs`; `PROBE_LINE_SCHEMA` below and
// `line::SCHEMA` there must stay equal.
//
// Secrets: a probe never prints a real MAC, unique id, calibration word or cardid byte.
// Identity-bearing facts are printed as a match against the placeholder value, not as the value
// itself.

#pragma once

#include <stdio.h>

// Schema of the line format. Bumped only when the grammar above changes.
#define PROBE_LINE_SCHEMA "passport-emu/probe-line/1"

// First line of every probe: names the probe and the schema its lines follow.
#define PROBE_BEGIN(name) \
    printf("PROBE|name=%s|schema=%s\n", (name), PROBE_LINE_SCHEMA)

// Last line of a probe run. `status` is `ok` when every step ran, `fail` otherwise. A probe that
// restarts on purpose (probe_reset) prints it only on the final boot of its sequence.
#define PROBE_END(name, status) printf("DONE|name=%s|status=%s\n", (name), (status))

// A step that could not run, with the reason. Not a failure by itself; the harness records it.
#define PROBE_NOTE(what, why) printf("NOTE|what=%s|why=%s\n", (what), (why))

// A step whose pass criterion did not hold. One FAIL line makes the run `fail`.
#define PROBE_FAIL(what, detail) printf("FAIL|what=%s|detail=%s\n", (what), (detail))
