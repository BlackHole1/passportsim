"""Print the order of entries into a set of watched functions, for the call-trace diff.

macOS only.

Run under `riscv32-esp-elf-gdb -batch -nx` against an oracle that is already listening for a
debugger. The oracle stays a black box: this script sets breakpoints by symbol name and prints
one `CALL <name>` line per entry, which is the format `pemu_verify::calltrace::CallTrace::parse`
reads. Nothing else about the oracle is inspected.

    WATCHED=bootloader_init,esp_image_load,call_start_cpu0,heap_caps_init,esp_flash_init,app_main \
    TRACE_OUT=/path/to/run.calls \
    riscv32-esp-elf-gdb -batch -nx -ex "file $ELF" -ex "target remote :1234" \
        -x tools/oracle/trace_calls.py

`WATCHED` is the watched set of the phase under comparison, about 60 names for a boot phase;
the six above are a minimal example. A name with no symbol is reported once on
stderr and skipped, so a missing breakpoint cannot look like a call that never happened.
"""

import os
import sys

import gdb  # provided by gdb's own Python runtime

WATCHED = [name.strip() for name in os.environ.get("WATCHED", "").split(",") if name.strip()]
TRACE_OUT = os.environ.get("TRACE_OUT")

if not WATCHED:
    sys.exit("trace_calls: set WATCHED to a comma-separated list of function names")

_out = open(TRACE_OUT, "w") if TRACE_OUT else sys.stdout


def _emit(name):
    _out.write("CALL %s\n" % name)
    _out.flush()


class _Entry(gdb.Breakpoint):
    """A breakpoint that records the entry and lets the program run on."""

    def __init__(self, name):
        super(_Entry, self).__init__(name, gdb.BP_BREAKPOINT, internal=False)
        self.silent = True
        self._name = name

    def stop(self):
        _emit(self._name)
        return False  # never stop the inferior: the order is the evidence, not the state


def main():
    gdb.execute("set pagination off")
    gdb.execute("set confirm off")
    for name in WATCHED:
        try:
            _Entry(name)
        except gdb.error as err:
            sys.stderr.write("trace_calls: no breakpoint for %s (%s)\n" % (name, err))
    gdb.execute("continue")


main()
