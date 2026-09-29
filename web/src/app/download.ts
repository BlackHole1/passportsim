// How the page words a download the Worker reports (`worker/download.ts`): the sentence the status
// line, the glass and the log show, from bytes received and the total `Content-Length` stated.

import type { DownloadProgress } from "../worker/download";
import type { Translate } from "./i18n";

export type { DownloadProgress };

type Unit = "B" | "kB" | "MB";

/** Decimal units, as a browser's download list shows them; one unit for both figures of a pair. */
function unitFor(bytes: number): Unit {
  return bytes >= 1_000_000 ? "MB" : bytes >= 1_000 ? "kB" : "B";
}

function inUnit(bytes: number, unit: Unit): string {
  switch (unit) {
    case "B":
      return String(bytes);
    case "kB":
      return (bytes / 1_000).toFixed(1);
    case "MB":
      return (bytes / 1_000_000).toFixed(1);
  }
}

/** `24.7 MB`. */
export function formatBytes(bytes: number): string {
  const unit = unitFor(bytes);
  return `${inUnit(bytes, unit)} ${unit}`;
}

/** Whole percent received, rounded down so 100 means every byte; `null` with no total. */
export function downloadPercent(received: number, total: number | null): number | null {
  if (total === null || received > total) {
    return null;
  }
  return total === 0 ? 100 : Math.floor((received * 100) / total);
}

/**
 * The figures of a download in progress: `7.5 / 24.7 MB (30%)` against a stated total, the bytes
 * alone without one, and nothing before the first header.
 */
export function downloadAmount(t: Translate, progress: Pick<DownloadProgress, "received" | "total">): string {
  const { received, total } = progress;
  const percent = downloadPercent(received, total);
  if (total !== null && percent !== null) {
    const unit = unitFor(total);
    return t("download.amount", { received: inUnit(received, unit), total: `${inUnit(total, unit)} ${unit}`, percent });
  }
  return received === 0 ? "" : formatBytes(received);
}

/** What is being downloaded: `Downloading the demo firmware`. */
export function downloadTitle(t: Translate, progress: Pick<DownloadProgress, "what">): string {
  return t(progress.what === "core" ? "download.core" : "download.firmware");
}

/** One line for the status and the log, in whichever state the download is. */
export function downloadText(t: Translate, progress: DownloadProgress): string {
  if (progress.error !== undefined) {
    return t(progress.what === "core" ? "download.coreFailed" : "download.firmwareFailed", { detail: progress.error });
  }
  if (progress.done) {
    return t(progress.what === "core" ? "download.coreDone" : "download.firmwareDone", { size: formatBytes(progress.received) });
  }
  const amount = downloadAmount(t, progress);
  return amount === "" ? downloadTitle(t, progress) : `${downloadTitle(t, progress)} ${amount}`;
}

/** Whether the download still has bytes to come. */
export function downloading(progress: DownloadProgress | null): progress is DownloadProgress {
  return progress !== null && !progress.done;
}
