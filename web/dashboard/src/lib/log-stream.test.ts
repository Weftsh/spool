import { describe, expect, it } from "vitest";
import { SseDecoder, decodeLogEvent } from "./log-stream";

/// The live build log is the one surface here with a wire format of our
/// own parsing, because `EventSource` cannot carry an `Authorization`
/// header and so cannot be used. Everything below is a way the hand-
/// written parser could be wrong while the page still looked like it
/// worked — a log that is subtly incorrect rather than obviously broken
/// is the failure mode this file exists to prevent.

/// Every frame a decoder produces from one string, for the cases where
/// the split does not matter.
function frames(text: string) {
  return new SseDecoder().push(text);
}

describe("SseDecoder", () => {
  it("dispatches a frame on the blank line that ends it", () => {
    expect(frames('event: chunk\ndata: {"text":"hi"}\n\n')).toEqual([
      { event: "chunk", data: '{"text":"hi"}' },
    ]);
  });

  it("holds an incomplete frame until the rest of it arrives", () => {
    // The whole reason this is a stateful object. A `read()` hands back
    // whatever bytes were in the socket, which is very often half a
    // line, and a decoder that parsed each read on its own would drop
    // every frame that straddled a boundary — most of them, on a busy
    // log.
    const d = new SseDecoder();
    expect(d.push("event: chu")).toEqual([]);
    expect(d.push("nk\ndata: {")).toEqual([]);
    expect(d.push('"text":"hi"}\n')).toEqual([]);
    expect(d.push("\n")).toEqual([{ event: "chunk", data: '{"text":"hi"}' }]);
  });

  it("strips exactly one space after the colon, and no more", () => {
    // The space is syntax, not value. Stripping none would make every
    // payload fail to parse as JSON; stripping greedily would eat the
    // indentation of a log line the server sent as data.
    expect(frames("data:  two spaces\n\n")[0].data).toBe(" two spaces");
  });

  it("joins several data lines with newlines, not by concatenation", () => {
    // A `data:` field cannot itself contain a newline, so a multi-line
    // payload arrives as several of them. Joining with "" would glue the
    // end of one log line onto the start of the next — the exact defect
    // the server sends chunks as JSON to avoid, reintroduced on the
    // client.
    expect(frames("data: one\ndata: two\n\n")[0].data).toBe("one\ntwo");
  });

  it("accepts CRLF as well as LF", () => {
    expect(frames("event: done\r\ndata: {}\r\n\r\n")).toEqual([
      { event: "done", data: "{}" },
    ]);
  });

  it("ignores the keep-alive comment", () => {
    // The server holds the socket open with a comment line. Read as a
    // field it would be one named "", which is harmless — but a frame
    // dispatched for it is not: the log would grow an empty event every
    // fifteen seconds forever.
    expect(frames(":\n\n: ping\n\n")).toEqual([]);
  });

  it("names an unlabelled frame `message`, as the format does", () => {
    expect(frames("data: bare\n\n")[0].event).toBe("message");
  });

  it("does not dispatch on a blank line that ends nothing", () => {
    const d = new SseDecoder();
    expect(d.push("\n\n\n")).toEqual([]);
    // …and the decoder is still usable afterwards, rather than having
    // been left mid-frame by the blanks.
    expect(d.push("event: queued\ndata: {}\n\n")).toEqual([
      { event: "queued", data: "{}" },
    ]);
  });

  it("returns several frames from one read", () => {
    // Two flushes can land in one TCP read, and a decoder that returned
    // only the first would leave the second in the buffer until the
    // *next* read — which on a job that has just finished never comes.
    const out = frames(
      'event: chunk\ndata: {"text":"a"}\n\nevent: done\ndata: {"state":"passed"}\n\n',
    );
    expect(out.map((f) => f.event)).toEqual(["chunk", "done"]);
  });

  it("resets between frames rather than accumulating", () => {
    const out = frames("event: chunk\ndata: a\n\ndata: b\n\n");
    expect(out).toEqual([
      { event: "chunk", data: "a" },
      // Neither the name nor the data of the first frame leaks into the
      // second.
      { event: "message", data: "b" },
    ]);
  });
});

describe("decodeLogEvent", () => {
  it("reads a chunk's text out of its JSON payload", () => {
    // JSON and not raw bytes, because the trailing newline of every
    // chunk is load-bearing: sent raw, an SSE `data:` field would lose
    // it and each chunk's last line would run into the next chunk's
    // first. This assertion is the client half of that contract.
    expect(
      decodeLogEvent({ event: "chunk", data: '{"text":"▶ Build\\n"}' }),
    ).toEqual({ kind: "chunk", text: "▶ Build\n" });
  });

  it("carries the verdict off a done frame", () => {
    expect(
      decodeLogEvent({ event: "done", data: '{"state":"failed"}' }),
    ).toEqual({ kind: "done", state: "failed" });
  });

  it("still ends the feed on a done frame with no state in it", () => {
    // A `done` means the feed is finished whatever else it says.
    // Treating this as unrecognised would leave the page tailing a
    // stream the server has already closed.
    expect(decodeLogEvent({ event: "done", data: "" })).toEqual({
      kind: "done",
      state: "",
    });
  });

  it("recognises the queued announcement", () => {
    expect(decodeLogEvent({ event: "queued", data: "{}" })).toEqual({
      kind: "queued",
    });
  });

  it("drops a chunk whose payload is not the JSON we expect", () => {
    // Rendering it would paste a raw brace or `[object Object]` into
    // somebody's build log, where it reads as output the build produced.
    expect(decodeLogEvent({ event: "chunk", data: "not json" })).toBeNull();
    expect(decodeLogEvent({ event: "chunk", data: "[1,2]" })).toBeNull();
    expect(decodeLogEvent({ event: "chunk", data: '{"text":7}' })).toBeNull();
  });

  it("ignores a frame this bundle has never heard of", () => {
    // A server newer than the page is a thing that happens. Inventing a
    // meaning for its words is how a log shows something that never
    // occurred; the honest answer is that the log does not grow.
    expect(decodeLogEvent({ event: "artifact", data: "{}" })).toBeNull();
    expect(decodeLogEvent({ event: "message", data: "{}" })).toBeNull();
  });
});
