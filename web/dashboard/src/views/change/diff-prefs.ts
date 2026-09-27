/// How a reviewer likes a diff laid out: one column or two.
///
/// A preference, not a fact about any change, so it lives in the browser
/// and follows the person from change to change. Unified is the default
/// because it is what every diff on this product looked like until the
/// split view existed, and because a phone has one column's width. A
/// choice a person made survives a reload; a browser that refuses
/// storage (a private window with it disabled) gets the default every
/// time and no error.
export type DiffStyle = "unified" | "split";

const KEY = "stratum-diff-style";

export function readDiffStyle(storage: Pick<Storage, "getItem"> | null): DiffStyle {
  try {
    return storage?.getItem(KEY) === "split" ? "split" : "unified";
  } catch {
    return "unified";
  }
}

export function writeDiffStyle(
  storage: Pick<Storage, "setItem"> | null,
  style: DiffStyle,
): void {
  try {
    storage?.setItem(KEY, style);
  } catch {
    // Storage refused; the choice holds for this page and no longer.
  }
}
