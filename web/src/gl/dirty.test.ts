import { describe, expect, test } from "bun:test";

import { DirtySpan, RowViews } from "./dirty";

describe("the dirty span", () => {
  test("starts empty", () => {
    const span = new DirtySpan(320);
    expect(span.pending).toBe(false);
    expect(span.firstRow).toBe(-1);
    expect(span.lastRow).toBe(-1);
  });

  test("holds one span as given", () => {
    const span = new DirtySpan(320);
    span.add(5, 7);
    expect([span.firstRow, span.lastRow]).toEqual([5, 7]);
  });

  test("unions spans a throttled upload skipped, including the rows between them", () => {
    const span = new DirtySpan(320);
    span.add(100, 101);
    span.add(0, 0);
    span.add(200, 200);
    expect([span.firstRow, span.lastRow]).toEqual([0, 200]);
  });

  test("clamps to the panel and ignores spans wholly outside it or reversed", () => {
    const span = new DirtySpan(320);
    span.add(-4, 2);
    span.add(310, 999);
    expect([span.firstRow, span.lastRow]).toEqual([0, 319]);
    const empty = new DirtySpan(320);
    empty.add(320, 400);
    empty.add(9, 3);
    empty.add(-9, -1);
    expect(empty.pending).toBe(false);
  });

  test("marks every row, then forgets everything once cleared", () => {
    const span = new DirtySpan(320);
    span.addAll();
    expect([span.firstRow, span.lastRow]).toEqual([0, 319]);
    span.clear();
    expect(span.pending).toBe(false);
    span.add(3, 3);
    expect([span.firstRow, span.lastRow]).toEqual([3, 3]);
  });
});

describe("row views", () => {
  const W = 4;
  const H = 3;

  test("start at the row asked for and alias the framebuffer", () => {
    const memory = new ArrayBuffer(64);
    const pixels = new Uint16Array(memory, 8, W * H);
    const views = new RowViews(W, H);
    const view = views.from(pixels, 1);
    expect(view.length).toBe(2 * W);
    pixels[W] = 0x1234;
    expect(view[0]).toBe(0x1234);
    expect(view.byteOffset).toBe(8 + W * 2);
  });

  test("create each view once, so steady-state uploads allocate nothing", () => {
    const pixels = new Uint16Array(W * H);
    const views = new RowViews(W, H);
    const a = views.from(pixels, 0);
    const b = views.from(pixels, 2);
    for (let frame = 0; frame < 100; frame += 1) {
      expect(views.from(pixels, 0)).toBe(a);
      expect(views.from(pixels, 2)).toBe(b);
    }
    expect(views.created).toBe(2);
  });

  test("are rebuilt when the buffer changes, as wasm memory growth does", () => {
    const views = new RowViews(W, H);
    const before = views.from(new Uint16Array(W * H), 0);
    const grown = new Uint16Array(W * H);
    grown[0] = 7;
    const after = views.from(grown, 0);
    expect(after).not.toBe(before);
    expect(after[0]).toBe(7);
    expect(views.created).toBe(2);
  });

  test("never reach past a short view", () => {
    const views = new RowViews(W, H);
    expect(views.from(new Uint16Array(W), 1).length).toBe(0);
  });
});
