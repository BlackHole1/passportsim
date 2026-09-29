// The firmware history, kept in this browser's IndexedDB only; nothing is sent anywhere. An entry
// keeps the bytes the image booted from, what the page shows about it, and a thumbnail. A merged
// image whose cardid window is not erased is a device backup and is never kept. Oldest entries are
// evicted first; a quota refusal evicts one more and retries.

import type { LoadedImage } from "./load";
import { TABLE_OFFSET } from "./load";
import { Store } from "./store";
import { LoadKind } from "../worker/layout";

export const MAX_ENTRIES = 12;

export const MAX_TOTAL_BYTES = 160 * 1024 * 1024;

/** The cardid partition: a merged image with anything but 0xFF here is a device backup. */
export const CARDID_WINDOW = { start: 0x356000, end: 0x35a000 } as const;

export interface HistoryFile {
  readonly name: string;
  readonly kind: LoadKind;
  readonly bytes: Uint8Array;
}

export interface HistoryEntry {
  /** A digest of the files, so loading the same firmware again finds its entry. */
  readonly id: string;
  readonly name: string;
  readonly size: number;
  readonly files: readonly { readonly name: string; readonly kind: LoadKind; readonly size: number }[];
  readonly buildId: string | null;
  readonly project: string | null;
  readonly version: string | null;
  readonly firstLoadedAt: number;
  readonly lastLoadedAt: number;
  readonly thumbnail: Uint8Array | null;
}

export interface HistoryBackend {
  list(): Promise<HistoryEntry[]>;
  /** Writes an entry, and its files when given; `files` `null` leaves the stored ones. */
  put(entry: HistoryEntry, files: readonly HistoryFile[] | null): Promise<void>;
  files(id: string): Promise<HistoryFile[] | null>;
  remove(id: string): Promise<void>;
  clear(): Promise<void>;
}

export type NotKept =
  | { readonly kind: "too-large"; readonly name: string; readonly size: number }
  | { readonly kind: "device-backup"; readonly name: string }
  | { readonly kind: "quota"; readonly name: string; readonly detail: string };

export interface HistoryView {
  readonly state: "opening" | "ready" | "unavailable";
  readonly entries: readonly HistoryEntry[];
  readonly reason: string | null;
  readonly notKept: NotKept | null;
  readonly error: string | null;
}

/** Two 32-bit FNV-1a lanes over every byte and each file's kind. A key, not a security hash. */
export function filesDigest(files: readonly { readonly kind: number; readonly bytes: Uint8Array }[]): string {
  let a = 0x811c9dc5;
  let b = 0x01000193 ^ 0x9e3779b9;
  const mix = (byte: number) => {
    a = Math.imul(a ^ byte, 0x01000193);
    b = Math.imul(b ^ byte, 0x5bd1e995);
    b ^= b >>> 15;
  };
  for (const file of files) {
    mix(file.kind);
    for (let i = 0; i < file.bytes.length; i += 1) {
      mix(file.bytes[i] as number);
    }
    mix(0xff);
  }
  const hex = (value: number) => (value >>> 0).toString(16).padStart(8, "0");
  return `${hex(a)}${hex(b)}`;
}

export function carriesCardId(bytes: Uint8Array): boolean {
  if (bytes.length <= CARDID_WINDOW.start || bytes.length < TABLE_OFFSET) {
    return false;
  }
  const end = Math.min(bytes.length, CARDID_WINDOW.end);
  for (let i = CARDID_WINDOW.start; i < end; i += 1) {
    if (bytes[i] !== 0xff) {
      return true;
    }
  }
  return false;
}

export function imageFiles(image: LoadedImage): HistoryFile[] {
  return image.assets.map((asset, index) => ({
    name: asset.file ?? `${image.name}-${index}.bin`,
    kind: asset.kind,
    bytes: asset.bytes,
  }));
}

export function imageOf(entry: HistoryEntry, files: readonly HistoryFile[]): LoadedImage {
  return {
    name: entry.name,
    assets: files.map((file) => ({ kind: file.kind, bytes: file.bytes, file: file.name })),
    notes: files.map((file) => `${file.name} from this browser's firmware history`),
  };
}

export function buildFieldsOf(status: unknown): { project: string | null; version: string | null } {
  const instances = (status as { instances?: unknown } | null)?.instances;
  const first = Array.isArray(instances) ? (instances[0] as { build?: { project?: unknown; version?: unknown } } | undefined) : undefined;
  const text = (value: unknown) => (typeof value === "string" && value.length > 0 ? value : null);
  return { project: text(first?.build?.project), version: text(first?.build?.version) };
}

export function isQuotaError(error: unknown): boolean {
  const name = (error as { name?: unknown } | null)?.name;
  return name === "QuotaExceededError" || name === "NS_ERROR_DOM_QUOTA_REACHED";
}

function errorText(error: unknown): string {
  if (error instanceof Error || (typeof error === "object" && error !== null && "message" in error)) {
    const { name, message } = error as { name?: string; message?: string };
    return name && message ? `${name}: ${message}` : (message ?? String(error));
  }
  return String(error);
}

export class FirmwareHistory {
  readonly store = new Store<HistoryView>({ state: "opening", entries: [], reason: null, notKept: null, error: null });
  private backend: HistoryBackend | null = null;
  private readonly opened: Promise<void>;

  constructor(open: (() => Promise<HistoryBackend>) | null, private readonly now: () => number = Date.now) {
    this.opened = this.open(open);
  }

  private async open(open: (() => Promise<HistoryBackend>) | null): Promise<void> {
    if (open === null) {
      this.store.update((view) => ({ ...view, state: "unavailable", reason: "this page has no storage for it" }));
      return;
    }
    try {
      const backend = await open();
      const entries = await backend.list();
      this.backend = backend;
      this.store.update((view) => ({ ...view, state: "ready", entries: sorted(entries) }));
    } catch (error) {
      this.store.update((view) => ({ ...view, state: "unavailable", reason: errorText(error) }));
    }
  }

  ready(): Promise<void> {
    return this.opened;
  }

  private setEntries(entries: readonly HistoryEntry[]): void {
    this.store.update((view) => ({ ...view, entries: sorted(entries) }));
  }

  private fail(error: unknown): void {
    this.store.update((view) => ({ ...view, error: errorText(error) }));
  }

  /** Keeps a booted firmware, moving a known one to the top. Returns its id, or `null` if not kept. */
  async record(image: LoadedImage): Promise<string | null> {
    await this.opened;
    const backend = this.backend;
    if (backend === null) {
      return null;
    }
    const files = imageFiles(image);
    const size = files.reduce((sum, file) => sum + file.bytes.length, 0);
    const refuse = (notKept: NotKept) => {
      this.store.update((view) => ({ ...view, notKept }));
      return null;
    };
    if (files.some((file) => file.kind === LoadKind.MergedFlash && carriesCardId(file.bytes))) {
      return refuse({ kind: "device-backup", name: image.name });
    }
    if (size > MAX_TOTAL_BYTES) {
      return refuse({ kind: "too-large", name: image.name, size });
    }
    const id = filesDigest(files);
    const now = this.now();
    let entries = [...this.store.get().entries];
    const known = entries.find((entry) => entry.id === id);
    try {
      if (known) {
        const entry = { ...known, name: image.name, lastLoadedAt: now };
        await backend.put(entry, null);
        this.setEntries(entries.map((one) => (one.id === id ? entry : one)));
        this.store.update((view) => ({ ...view, notKept: null, error: null }));
        return id;
      }
      const entry: HistoryEntry = {
        id,
        name: image.name,
        size,
        files: files.map((file) => ({ name: file.name, kind: file.kind, size: file.bytes.length })),
        buildId: null,
        project: null,
        version: null,
        firstLoadedAt: now,
        lastLoadedAt: now,
        thumbnail: null,
      };
      entries = sorted(entries);
      while (entries.length >= MAX_ENTRIES || entries.reduce((sum, one) => sum + one.size, 0) + size > MAX_TOTAL_BYTES) {
        const oldest = entries.pop();
        if (!oldest) {
          break;
        }
        await backend.remove(oldest.id);
      }
      for (;;) {
        try {
          await backend.put(entry, files);
          break;
        } catch (error) {
          if (!isQuotaError(error)) {
            throw error;
          }
          const oldest = entries.pop();
          if (!oldest) {
            this.setEntries(entries);
            return refuse({ kind: "quota", name: image.name, detail: errorText(error) });
          }
          await backend.remove(oldest.id);
        }
      }
      this.setEntries([entry, ...entries]);
      this.store.update((view) => ({ ...view, notKept: null, error: null }));
      return id;
    } catch (error) {
      this.setEntries(entries);
      this.fail(error);
      return null;
    }
  }

  async annotate(id: string, patch: Partial<Pick<HistoryEntry, "buildId" | "project" | "version" | "thumbnail">>): Promise<void> {
    const backend = this.backend;
    const current = this.store.get().entries.find((entry) => entry.id === id);
    if (backend === null || current === undefined) {
      return;
    }
    const entry = { ...current, ...patch };
    try {
      await backend.put(entry, null);
      this.setEntries(this.store.get().entries.map((one) => (one.id === id ? entry : one)));
    } catch (error) {
      this.fail(error);
    }
  }

  async files(id: string): Promise<HistoryFile[] | null> {
    if (this.backend === null) {
      return null;
    }
    try {
      return await this.backend.files(id);
    } catch (error) {
      this.fail(error);
      return null;
    }
  }

  async image(id: string): Promise<LoadedImage | null> {
    const entry = this.store.get().entries.find((one) => one.id === id);
    const files = entry === undefined ? null : await this.files(id);
    return entry === undefined || files === null ? null : imageOf(entry, files);
  }

  async remove(id: string): Promise<void> {
    if (this.backend === null) {
      return;
    }
    try {
      await this.backend.remove(id);
      this.setEntries(this.store.get().entries.filter((entry) => entry.id !== id));
    } catch (error) {
      this.fail(error);
    }
  }

  async clear(): Promise<void> {
    if (this.backend === null) {
      return;
    }
    try {
      await this.backend.clear();
      this.setEntries([]);
      this.store.update((view) => ({ ...view, notKept: null, error: null }));
    } catch (error) {
      this.fail(error);
    }
  }
}

function sorted(entries: readonly HistoryEntry[]): HistoryEntry[] {
  return [...entries].sort((a, b) => b.lastLoadedAt - a.lastLoadedAt);
}

export function memoryBackend(): HistoryBackend & { readonly entries: Map<string, HistoryEntry>; readonly blobs: Map<string, HistoryFile[]> } {
  const entries = new Map<string, HistoryEntry>();
  const blobs = new Map<string, HistoryFile[]>();
  return {
    entries,
    blobs,
    list: () => Promise.resolve([...entries.values()]),
    put: (entry, files) => {
      entries.set(entry.id, entry);
      if (files !== null) {
        blobs.set(entry.id, [...files]);
      }
      return Promise.resolve();
    },
    files: (id) => Promise.resolve(blobs.get(id) ?? null),
    remove: (id) => {
      entries.delete(id);
      blobs.delete(id);
      return Promise.resolve();
    },
    clear: () => {
      entries.clear();
      blobs.clear();
      return Promise.resolve();
    },
  };
}

export const DB_NAME = "passportsim-firmware-history";
const DB_VERSION = 1;

function request<T>(req: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => {
      resolve(req.result);
    };
    req.onerror = () => {
      reject(req.error ?? new Error("IndexedDB request failed"));
    };
  });
}

function done(tx: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => {
      resolve();
    };
    tx.onabort = () => {
      reject(tx.error ?? new Error("IndexedDB transaction aborted"));
    };
    tx.onerror = () => {
      reject(tx.error ?? new Error("IndexedDB transaction failed"));
    };
  });
}

/** Opens the history in IndexedDB; rejects with the browser's reason where it cannot. */
export async function openIndexedDb(factory: IDBFactory | undefined): Promise<HistoryBackend> {
  if (factory === undefined) {
    throw new Error("this browser offers no IndexedDB");
  }
  const openRequest = factory.open(DB_NAME, DB_VERSION);
  openRequest.onupgradeneeded = () => {
    const db = openRequest.result;
    if (!db.objectStoreNames.contains("entries")) {
      db.createObjectStore("entries", { keyPath: "id" });
    }
    if (!db.objectStoreNames.contains("files")) {
      db.createObjectStore("files", { keyPath: "id" });
    }
  };
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    openRequest.onsuccess = () => {
      resolve(openRequest.result);
    };
    openRequest.onerror = () => {
      reject(openRequest.error ?? new Error("IndexedDB would not open"));
    };
    openRequest.onblocked = () => {
      reject(new Error("IndexedDB is held open by another tab of an older page"));
    };
  });
  return {
    async list() {
      const tx = db.transaction("entries", "readonly");
      const all = await request(tx.objectStore("entries").getAll() as IDBRequest<HistoryEntry[]>);
      return all;
    },
    async put(entry, files) {
      const tx = db.transaction(files === null ? ["entries"] : ["entries", "files"], "readwrite");
      tx.objectStore("entries").put(entry);
      if (files !== null) {
        tx.objectStore("files").put({ id: entry.id, files });
      }
      await done(tx);
    },
    async files(id) {
      const tx = db.transaction("files", "readonly");
      const row = await request(tx.objectStore("files").get(id) as IDBRequest<{ files: HistoryFile[] } | undefined>);
      return row?.files ?? null;
    },
    async remove(id) {
      const tx = db.transaction(["entries", "files"], "readwrite");
      tx.objectStore("entries").delete(id);
      tx.objectStore("files").delete(id);
      await done(tx);
    },
    async clear() {
      const tx = db.transaction(["entries", "files"], "readwrite");
      tx.objectStore("entries").clear();
      tx.objectStore("files").clear();
      await done(tx);
    },
  };
}
