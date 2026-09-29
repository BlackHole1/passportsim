// Turns a drag, a file input or a directory input into a `Drop`, so all three reach the same
// `loadDrop` and a test exercises what a drag does.

import type { Drop, DropFile } from "./load";

export interface TransferLike {
  readonly items?: ArrayLike<DataTransferItem> | null;
  readonly files?: ArrayLike<File> | null;
}

export function dropFile(file: File, relative: string): DropFile {
  return {
    path: relative,
    size: file.size,
    read: async () => new Uint8Array(await file.arrayBuffer()),
  };
}

/**
 * What an `<input type="file">` holds. A `webkitdirectory` input's `webkitRelativePath` starts
 * with the picked directory, which becomes the drop's root; a plain input gives a rootless drop.
 */
export function dropFromFiles(files: ArrayLike<File>): Drop {
  const list = Array.from(files);
  const roots = new Set<string>();
  const out: DropFile[] = [];
  for (const file of list) {
    const relative = file.webkitRelativePath ?? "";
    if (relative === "") {
      out.push(dropFile(file, file.name));
      continue;
    }
    const segments = relative.split("/");
    roots.add(segments[0] ?? "");
    out.push(dropFile(file, segments.slice(1).join("/") || file.name));
  }
  // Two roots cannot come from one input; if they did, the files are treated as rootless.
  const root = roots.size === 1 ? ([...roots][0] ?? null) : null;
  return { root: root === "" ? null : root, files: out };
}

function entryFile(entry: FileSystemFileEntry): Promise<File> {
  return new Promise((resolve, reject) => {
    entry.file(resolve, reject);
  });
}

/** One `readEntries` call; a directory reader answers in batches until it answers none. */
function readBatch(reader: FileSystemDirectoryReader): Promise<FileSystemEntry[]> {
  return new Promise((resolve, reject) => {
    reader.readEntries(resolve, reject);
  });
}

async function walkEntry(entry: FileSystemEntry, prefix: string, out: DropFile[], limit: number): Promise<void> {
  if (out.length >= limit) {
    return;
  }
  if (entry.isFile) {
    const file = await entryFile(entry as FileSystemFileEntry);
    out.push(dropFile(file, `${prefix}${entry.name}`));
    return;
  }
  if (!entry.isDirectory) {
    return;
  }
  const reader = (entry as FileSystemDirectoryEntry).createReader();
  for (;;) {
    const batch = await readBatch(reader);
    if (batch.length === 0) {
      return;
    }
    for (const child of batch) {
      await walkEntry(child, `${prefix}${entry.name}/`, out, limit);
    }
  }
}

/**
 * The drop a `DataTransfer` carries. A directory drag is only visible through `webkitGetAsEntry`,
 * so that is tried first. The walk falls back to `files` when it yields nothing (no entry API, or
 * WebKit refusing a scripted transfer's file); once it has produced a file, an error propagates,
 * since `files` would be half of the directory.
 */
export async function dropFromTransfer(transfer: TransferLike, limit: number): Promise<Drop> {
  const items = Array.from(transfer.items ?? []);
  const entries = items
    .map((item) => (typeof item.webkitGetAsEntry === "function" ? item.webkitGetAsEntry() : null))
    .filter((entry): entry is FileSystemEntry => entry !== null);
  const only = entries.length === 1 ? entries[0] : undefined;
  const out: DropFile[] = [];
  try {
    if (only !== undefined && only.isDirectory) {
      const reader = (only as FileSystemDirectoryEntry).createReader();
      for (;;) {
        const batch = await readBatch(reader);
        if (batch.length === 0) {
          break;
        }
        for (const child of batch) {
          await walkEntry(child, "", out, limit);
        }
      }
      if (out.length > 0) {
        return { root: only.name, files: out };
      }
    } else {
      for (const entry of entries) {
        await walkEntry(entry, "", out, limit);
      }
      if (out.length > 0) {
        return { root: null, files: out };
      }
    }
  } catch (error) {
    if (out.length > 0) {
      throw error;
    }
  }
  return dropFromFiles(transfer.files ?? []);
}

export const WALK_LIMIT = 8192;
