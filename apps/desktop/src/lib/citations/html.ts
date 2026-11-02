import { parseCitations, splitAtCrossingMarkers, type TextSpan } from "./parse";
import { resolveCitation, type SegmentIndex } from "./resolve";
import {
  pendingPillToken,
  splitCitations,
  toPendingPillHtml,
  toPillHtml,
} from "./render";

// Text in these is shown literally (code), is already a reference (pills),
// or can't hold a pill (a pill nested in a link is invalid UI).
const SKIP_TEXT_IN = "code, pre, a, [data-segment-ref]";
const PILL = "span[data-segment-ref]";
// Elements text flows through within one line (a `<br>` ends it), so a marker can continue
// across their edges (`<strong>[[seg:</strong>ID]]`).
const INLINE =
  "a, abbr, b, bdi, bdo, cite, code, data, del, dfn, em, font, i, img, ins, kbd, mark, q, s, samp, small, span, strike, strong, sub, sup, time, u, var, wbr";

function blockOf(node: Node): Element | null {
  let el = node.parentElement;
  while (el?.matches(INLINE)) el = el.parentElement;
  return el;
}

/**
 * Citation work in stored note HTML, found by walking the DOM rather than
 * matching the raw string: only visible text nodes outside code and pills
 * are read, never attribute values. Same `TreeWalker` approach as
 * `replaceTextInHtml` in `lib/ai-tools.ts`. Text is read a line at a time
 * across inline elements, so a marker split by formatting is found whole and
 * kept literal (`splitAtCrossingMarkers`). Returns each text node holding a
 * marker with its spans.
 */
function citationTextNodes(doc: Document): Array<[Text, TextSpan[]]> {
  const lines: Node[][] = [];
  let line: Node[] = [];
  let lineBlock: Element | null = null;
  const walker = doc.createTreeWalker(
    doc.body,
    NodeFilter.SHOW_TEXT | NodeFilter.SHOW_ELEMENT,
  );
  while (walker.nextNode()) {
    const node = walker.currentNode;
    const isLeaf =
      node.nodeType === Node.TEXT_NODE ||
      (node as Element).matches("br, img, wbr");
    if (!isLeaf) continue;
    const block = blockOf(node);
    const isBreak = node.nodeName === "BR";
    if ((isBreak || block !== lineBlock) && line.length > 0) {
      lines.push(line);
      line = [];
    }
    lineBlock = block;
    if (!isBreak) line.push(node);
  }
  if (line.length > 0) lines.push(line);

  const out: Array<[Text, TextSpan[]]> = [];
  for (const nodes of lines) {
    const pieces = nodes.map((n) =>
      n.nodeType === Node.TEXT_NODE ? (n as Text).data : "",
    );
    splitAtCrossingMarkers(pieces).forEach((spans, i) => {
      const node = nodes[i];
      if (node.nodeType !== Node.TEXT_NODE) return;
      if (node.parentElement?.closest(SKIP_TEXT_IN)) return;
      if (spans.some((s) => !s.literal && parseCitations(s.text).length > 0)) {
        out.push([node as Text, spans]);
      }
    });
  }
  return out;
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
 * pending pills whose segment now exists. An ID-shaped marker that doesn't
 * resolve is kept as a pending pill, never removed. Text in code, `pre`,
 * links and existing pills is left alone, as are attribute values. Returns
 * `html` itself when nothing changed, so callers can compare to decide
 * whether to persist.
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

  for (const [node, spans] of citationTextNodes(doc)) {
    const frag = doc.createDocumentFragment();
    let nodeChanged = false;
    for (const span of spans) {
      if (span.literal) {
        frag.append(doc.createTextNode(span.text));
        continue;
      }
      const { parts } = splitCitations(span.text, index, { keepUnresolved: true });
      if (parts.length !== 1 || parts[0].kind !== "text" || parts[0].text !== span.text) {
        nodeChanged = true;
      }
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
    }
    if (!nodeChanged) continue;
    node.replaceWith(frag);
    changed = true;
  }

  return changed ? doc.body.innerHTML : html;
}
