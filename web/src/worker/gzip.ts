// A firmware file may arrive gzip-compressed. The web bundle stores the demo `.pebundle` that way:
// Cloudflare does not compress an `application/octet-stream` response, and the plain bundle is
// four times the size on the wire. A file saved from the site and dropped back arrives the same way.

/** Whether `bytes` start with the gzip magic (RFC 1952). */
export function isGzip(bytes: Uint8Array): boolean {
  return bytes.length >= 2 && bytes[0] === 0x1f && bytes[1] === 0x8b;
}

/** `bytes` inflated when they are gzip, otherwise `bytes` itself. */
export async function inflateIfGzip(bytes: Uint8Array): Promise<Uint8Array> {
  if (!isGzip(bytes)) {
    return bytes;
  }
  const inflated = new Blob([bytes as Uint8Array<ArrayBuffer>]).stream().pipeThrough(new DecompressionStream("gzip"));
  return new Uint8Array(await new Response(inflated).arrayBuffer());
}
