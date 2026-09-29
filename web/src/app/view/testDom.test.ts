import { expect, test } from "bun:test";
import { installDom } from "./testDom";

const window = installDom();
const document = window.document as unknown as Document;

test("a failed assertion on a node prints its markup, and fails", () => {
  const mount = document.createElement("div");
  mount.innerHTML = '<p data-probe="1">text</p>';
  document.body.appendChild(mount);
  let message = "";
  try {
    expect(mount.querySelector("[data-probe]")).toBeNull();
  } catch (error) {
    message = String(error);
  }
  mount.remove();
  expect(message).toContain('<p data-probe="1">text</p>');
  // Without the markup printer the message is megabytes of the window's object graph.
  expect(message.length).toBeLessThan(1_000);
});
