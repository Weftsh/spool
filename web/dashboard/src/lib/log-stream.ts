/// Reading a hosted job's log as it is produced.
///
/// The server's live log is a server-sent-event feed, and this file is
/// the half of it that has no `fetch` in it: bytes in, events out. That
/// split is the whole reason it is a file rather than a closure inside
/// `api.ts` — the wire format has four edge cases that each produced a
/// wrong log the first time somebody wrote one of these by hand, and
/// none of them is reachable from a test that needs a socket.
///
/// **`EventSource` cannot be used here.** It sends no headers, so a
/// token session would ask for the feed unauthenticated and be refused;
/// the fetch-plus-reader shape below is what carries `Authorization`.
/// That means we own the parsing, which is what the rest of this file
/// is.

/// One dispatched frame.
export interface SseEvent {
  /// The frame's `event:` field, or `message` — the name the spec gives
  /// a frame that carried none.
  event: string;
  /// Every `data:` line, rejoined with newlines. Rejoined and **not**
  /// concatenated: the wire format splits a multi-line payload across
  /// several `data:` lines precisely because a `data:` field cannot
  /// contain a newline, and a reader that joined them with `""` would
  /// silently glue two log lines together.
  data: string;
}

/// A running parse of an SSE byte stream.
///
/// Stateful because the transport is: a `read()` hands back whatever
/// arrived, which is very often half a line. Four things this has to get
/// right, all of which have produced a subtly wrong log elsewhere:
///
/// - a frame split across two reads — hence the retained buffer;
/// - `\r\n` as well as `\n`, because the format permits both;
/// - the leading space after `field:` is part of the syntax, not the
///   value, so `data: {}` carries `{}` and not ` {}`;
/// - comment lines (`:` first) are the keep-alive the server sends to
///   hold the socket open, and reading one as a field would inject an
///   empty frame into the log every fifteen seconds.
export class SseDecoder {
  private buf = "";
  private name: string | null = null;
  private data: string[] = [];
  /// Whether the frame being accumulated carried anything at all. A
  /// blank line with nothing before it is a no-op rather than an empty
  /// event — a stream that opens with one, which is legal, must not
  /// dispatch.
  private started = false;

  /// Feed the next piece of text. Returns whatever frames completed.
  push(text: string): SseEvent[] {
    this.buf += text;
    const out: SseEvent[] = [];
    for (;;) {
      const nl = this.buf.indexOf("\n");
      if (nl === -1) break;
      let line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (line.endsWith("\r")) line = line.slice(0, -1);
      const done = this.line(line);
      if (done) out.push(done);
    }
    return out;
  }

  private line(line: string): SseEvent | null {
    if (line === "") {
      if (!this.started) return null;
      const ev = { event: this.name ?? "message", data: this.data.join("\n") };
      this.name = null;
      this.data = [];
      this.started = false;
      return ev;
    }
    // The keep-alive. Dropped rather than parsed as a field named "".
    if (line.startsWith(":")) return null;
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") {
      this.name = value;
      this.started = true;
    } else if (field === "data") {
      this.data.push(value);
      this.started = true;
    }
    // `id` and `retry` are the other two fields in the format. Neither
    // means anything to this feed — the server never resumes a log from
    // a Last-Event-ID — so they are read and discarded rather than
    // treated as data.
    return null;
  }
}

/// What one of this feed's frames means.
///
/// `null` for a frame this bundle does not recognise, which is the same
/// rule the Checks tab applies to an unknown run state: a newer server
/// is a thing that happens, and inventing a meaning for its words is how
/// a log ends up showing something that never happened. An unrecognised
/// frame is ignored; the log simply does not grow.
export type LogEvent =
  /// The job has not been picked up yet. Sent once, and only while the
  /// job is genuinely waiting for a runner.
  | { kind: "queued" }
  /// More output. `text` already has its newlines: the server sends the
  /// chunk as JSON rather than as raw bytes for exactly that reason.
  | { kind: "chunk"; text: string }
  /// The job is over and the feed has closed itself. `state` is the
  /// verdict it ended on.
  | { kind: "done"; state: string };

export function decodeLogEvent(ev: SseEvent): LogEvent | null {
  if (ev.event === "queued") return { kind: "queued" };
  if (ev.event === "chunk") {
    // A chunk whose payload is not the JSON we expect is dropped rather
    // than rendered. `String(...)` on a parse failure would paste
    // `[object Object]` or a raw brace into somebody's build log, which
    // reads as the build having produced it.
    const text = jsonField(ev.data, "text");
    return text === null ? null : { kind: "chunk", text };
  }
  if (ev.event === "done") {
    // The state is worth having and not worth failing over: a `done`
    // that arrived without one still means the feed is finished, and
    // treating it as unrecognised would leave the page tailing a stream
    // that has already closed.
    return { kind: "done", state: jsonField(ev.data, "state") ?? "" };
  }
  return null;
}

/// One string field out of a JSON object, or `null` if the payload is
/// not JSON, is not an object, or the field is not a string.
function jsonField(data: string, field: string): string | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(data);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const value = (parsed as Record<string, unknown>)[field];
  return typeof value === "string" ? value : null;
}
