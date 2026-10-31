import { parseCitations } from "./parse";
import { resolveCitation, type SegmentIndex } from "./resolve";
import {
  pendingPillToken,
  splitCitations,
  toPendingPillHtml,
  toPillHtml,
} from "./render";

// Text in these is shown literally (code) or already a reference (pills).
const SKIP_TEXT_IN = "code, pre, [data-segment-ref]";
const PILL = "span[data-segment-ref]";

/**
 * Citation work in stored note HTML, found by walking the DOM rather than
 * matching the raw string: only visible text nodes outside code and pills
 * are read, never attribute values. Same `TreeWalker(SHOW_TEXT)` approach as
 * `replaceTextInHtml` in `lib/ai-tools.ts`.
 */
function citationTextNodes(doc: Document): Text[] {
  const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
  const nodes: Text[] = [];
  while (walker.nextNode()) {
    const node = walker.currentNode as Text;
    if (node.parentElement?.closest(SKIP_TEXT_IN)) continue;
    if (parseCitations(node.data).length > 0) nodes.push(node);
  }
  return nodes;
}

function pendingPills(doc: Document): Element[] {
  return [...doc.body.querySelectorAll(PILL)].filter(
    (el) => !el.getAttribute("data-timestamp"),
  );
}

function parseHtml(html: string): Document {
  return new DOMParser().parseFromString(html, "text/html");
}

function fragmentFromHtml(doc: Document, html: string): DocumentFragment {
  const template = doc.createElement("template");
  template.innerHTML = html;
  return template.content;
}

/**
 * Whether `html` holds anything `convertCitationsInHtml` would act on: a
 * citation marker in visible text, or a pending pill to re-resolve.
 */
export function hasUnconvertedCitations(html: string): boolean {
  const doc = parseHtml(html);
  return pendingPills(doc).length > 0 || citationTextNodes(doc).length > 0;
}

/**
 * Convert citation markers in stored note HTML into pills, and re-resolve
 * pending pills whose segment now exists. A marker that doesn't resolve is
 * kept as a pending pill, never removed. Text in code, `pre` and existing
 * pills is left alone, as are attribute values. Returns `html` itself when
 * nothing changed, so callers can compare to decide whether to persist.
 */
export function convertCitationsInHtml(
  html: string,
  index: SegmentIndex,
): string {
  const doc = parseHtml(html);
  let changed = false;

  for (const el of pendingPills(doc)) {
    const token = pendingPillToken(
      el.getAttribute("data-segment-id") ?? "",
      Number(el.getAttribute("data-offset") ?? "0"),
    );
    const ref = resolveCitation(token, index);
    if (!ref) continue;
    el.replaceWith(fragmentFromHtml(doc, toPillHtml(ref)));
    changed = true;
  }

  for (const node of citationTextNodes(doc)) {
    const { parts } = splitCitations(node.data, index, { keepUnresolved: true });
    const frag = doc.createDocumentFragment();
    for (const part of parts) {
      if (part.kind === "text") {
        frag.append(doc.createTextNode(part.text));
      } else {
        frag.append(
          fragmentFromHtml(
            doc,
            part.kind === "ref"
              ? toPillHtml(part.ref)
              : toPendingPillHtml(part.token),
          ),
        );
      }
    }
    node.replaceWith(frag);
    changed = true;
  }

  return changed ? doc.body.innerHTML : html;
}
