// The wasm32 leg of the determinism harness, over the ABI the Worker uses: one boot of a merged
// image (or a .pebundle) to a console line through `pemu_new`, `pemu_load`, `pemu_build`,
// `pemu_call` (`@stops`, then `@report`) and `pemu_run`, printed as one line prefixed `REPORT `.
// The line is `pemu_machine::determinism::report`, which the native legs of the milestone tests
// compute from the same inputs, so the legs agree exactly when the machine is host independent.
//
// One plain script serves Node and the macOS `jsc` shell: Node gets `require` and `process`,
// `jsc` gets `readFile`, `print` and `arguments`, and neither is asked for a text codec.
//
// Usage: <engine> abi_parity.cjs [--] <pemu_wasm.wasm> <image> <pattern-hex> <prefix 0|1>
//        <max_insns> <max_block_insns> <max_slice> <poll_ff 0|1> [<script.json>]
//
// The optional script is `{"inputs": [{at, event}], "until_ps": "<ps>"}`: the
// inputs are journaled with `pemu_input` before the run, and `until_ps` bounds it.
//
// The pattern travels as hex of its UTF-8 bytes, so no engine has to agree on a text encoding.
"use strict";

const isNode =
  typeof process !== "undefined" && process.versions != null && process.versions.node != null;

// The `jsc` shell's script arguments, read at the top level: inside a function `arguments` is
// that function's own.
const shellArgs =
  typeof scriptArgs !== "undefined"
    ? Array.from(scriptArgs)
    : typeof arguments !== "undefined"
      ? Array.from(arguments)
      : [];

function argv() {
  if (isNode) {
    return process.argv.slice(2);
  }
  return shellArgs;
}

function readBytes(path) {
  if (isNode) {
    return new Uint8Array(require("fs").readFileSync(path));
  }
  return new Uint8Array(readFile(path, "binary"));
}

function readText(path) {
  if (isNode) {
    return require("fs").readFileSync(path, "utf8");
  }
  return readFile(path);
}

function emit(line) {
  if (isNode) {
    process.stdout.write(line + "\n");
  } else {
    print(line);
  }
}

function fromHex(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(hex.substr(i * 2, 2), 16);
  }
  return out;
}

/** ASCII text as bytes; every string this script builds itself is ASCII. */
function ascii(text) {
  const out = new Uint8Array(text.length);
  for (let i = 0; i < text.length; i++) {
    out[i] = text.charCodeAt(i);
  }
  return out;
}

function concat(parts) {
  let len = 0;
  for (const p of parts) len += p.length;
  const out = new Uint8Array(len);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/**
 * UTF-8 bytes as the inside of a JSON string: quote, backslash and control bytes escaped, every
 * other byte (non-ASCII UTF-8 included) passed through, which JSON text allows.
 */
function jsonStringBytes(bytes) {
  const out = [];
  for (const b of bytes) {
    if (b === 0x22 || b === 0x5c) {
      out.push(0x5c, b);
    } else if (b < 0x20) {
      for (const c of ascii("\\u00" + (b < 16 ? "0" : "") + b.toString(16))) out.push(c);
    } else {
      out.push(b);
    }
  }
  return new Uint8Array(out);
}

function main() {
  const args = argv().filter((a) => a !== "--");
  if (args.length !== 8 && args.length !== 9) {
    emit("ERROR expected 8 or 9 arguments, got " + args.length);
    return;
  }
  const [wasmPath, imagePath, patternHex, prefix, maxInsns, block, slice, pollFf, scriptPath] =
    args;
  const x = new WebAssembly.Instance(new WebAssembly.Module(readBytes(wasmPath)), {}).exports;

  // Copies bytes in, runs `body(ptr, len)`, frees them.
  const withBytes = (bytes, body) => {
    const ptr = bytes.length === 0 ? 0 : x.pemu_alloc(bytes.length);
    if (bytes.length !== 0 && ptr === 0) {
      throw new Error("pemu_alloc(" + bytes.length + ") failed");
    }
    new Uint8Array(x.memory.buffer).set(bytes, ptr);
    try {
      return body(ptr, bytes.length);
    } finally {
      if (bytes.length !== 0) x.pemu_free(ptr, bytes.length);
    }
  };
  // Reads a result header (ptr, len, status: little-endian u32), frees it, and throws on a failure.
  const result = (res) => {
    const header = new DataView(x.memory.buffer, res, 12);
    const ptr = header.getUint32(0, true);
    const len = header.getUint32(4, true);
    const status = header.getUint32(8, true);
    const bytes = new Uint8Array(x.memory.buffer).slice(ptr, ptr + len);
    x.pemu_result_free(res);
    if (status !== 0) {
      let text = "";
      for (const b of bytes) text += String.fromCharCode(b);
      throw new Error("status " + status + ": " + text);
    }
    return bytes;
  };
  const call = (handle, json) =>
    result(withBytes(json, (ptr, len) => x.pemu_call(handle, ptr, len)));

  const config = ascii(
    '{"max_block_insns":' +
      Number(block) +
      ',"max_slice":' +
      BigInt(slice).toString() +
      ',"poll_ff":' +
      (Number(pollFf) !== 0 ? "true" : "false") +
      "}",
  );
  const builder = withBytes(config, (ptr, len) => x.pemu_new(ptr, len));
  withBytes(readBytes(imagePath), (ptr, len) => result(x.pemu_load(builder, 1, ptr, len)));
  const built = result(x.pemu_build(builder));
  const handle = new DataView(built.buffer).getUint32(0, true);

  const kind = Number(prefix) !== 0 ? "prefix" : "contains";
  call(
    handle,
    concat([
      ascii('{"cmd":"@stops","args":{"matchers":[{"id":1,"serial":{"stream":"usj","' + kind + '":"'),
      jsonStringBytes(fromHex(patternHex)),
      ascii('"}}]}}'),
    ]),
  );
  let until = -1n;
  if (scriptPath !== undefined) {
    const script = JSON.parse(readText(scriptPath));
    // The script is ASCII JSON, so re-serializing its inputs gives the bytes `pemu_input` reads.
    result(withBytes(ascii(JSON.stringify(script.inputs)), (ptr, len) => x.pemu_input(handle, ptr, len)));
    until = BigInt(script.until_ps);
  }
  // `pemu_run` answers the stop discriminant, or `NO_MACHINE` (u32::MAX, which an i32 result
  // carries as -1) for a handle that names no machine; the report would then describe nothing.
  const stop = x.pemu_run(handle, until, BigInt(maxInsns)) >>> 0;
  if (stop === 0xffffffff) {
    throw new Error("pemu_run: handle " + handle + " names no machine");
  }
  // The report is ASCII (hex digests, decimal numbers and a Debug-rendered stop reason) inside
  // a JSON object, so a byte-per-char decode is exact.
  const answer = call(handle, ascii('{"cmd":"@report"}'));
  let text = "";
  for (const b of answer) text += String.fromCharCode(b);
  emit("REPORT " + JSON.parse(text).report);
  x.pemu_drop(handle);
}

try {
  main();
} catch (e) {
  emit("ERROR " + e);
}
