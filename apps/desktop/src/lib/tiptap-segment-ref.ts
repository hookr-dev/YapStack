import { Node, mergeAttributes } from "@tiptap/core";

export interface SegmentReferenceOptions {
  HTMLAttributes: Record<string, unknown>;
}

declare module "@tiptap/core" {
  interface Commands<ReturnType> {
    segmentReference: {
      insertSegmentReference: (attrs: {
        segmentId: string;
        timestamp: string;
        offsetSeconds: number;
      }) => ReturnType;
    };
  }
}

export const SegmentReference = Node.create<SegmentReferenceOptions>({
  name: "segmentReference",
  group: "inline",
  inline: true,
  atom: true,

  renderMarkdown(node) {
    return node.attrs?.timestamp ? `[${node.attrs.timestamp}]` : "";
  },

  addOptions() {
    return { HTMLAttributes: {} };
  },

  addAttributes() {
    return {
      segmentId: { default: null, parseHTML: (el: HTMLElement) => el.getAttribute("data-segment-id") },
      timestamp: { default: null, parseHTML: (el: HTMLElement) => el.getAttribute("data-timestamp") },
      offsetSeconds: {
        default: 0,
        parseHTML: (el: HTMLElement) => parseFloat(el.getAttribute("data-offset") ?? "0"),
      },
    };
  },

  parseHTML() {
    return [{ tag: "span[data-segment-ref]" }];
  },

  renderHTML({ node, HTMLAttributes }) {
    return [
      "span",
      mergeAttributes(this.options.HTMLAttributes, HTMLAttributes, {
        "data-segment-ref": "",
        "data-segment-id": node.attrs.segmentId,
        "data-timestamp": node.attrs.timestamp,
        "data-offset": String(node.attrs.offsetSeconds),
      }),
      node.attrs.timestamp,
    ];
  },

  addNodeView() {
    return ({ node }) => {
      const dom = document.createElement("span");
      dom.setAttribute("data-segment-ref", "");
      dom.setAttribute("data-segment-id", node.attrs.segmentId);
      dom.setAttribute("data-timestamp", node.attrs.timestamp);
      dom.setAttribute("data-offset", String(node.attrs.offsetSeconds));
      dom.contentEditable = "false";

      // Quote icon SVG (matches lucide Quote at 10px)
      const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
      svg.setAttribute("width", "10");
      svg.setAttribute("height", "10");
      svg.setAttribute("viewBox", "0 0 24 24");
      svg.setAttribute("fill", "none");
      svg.setAttribute("stroke", "currentColor");
      svg.setAttribute("stroke-width", "2");
      svg.setAttribute("stroke-linecap", "round");
      svg.setAttribute("stroke-linejoin", "round");
      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute("d", "M3 21c3 0 7-1 7-8V5c0-1.25-.756-2.017-2-2H4c-1.25 0-2 .75-2 1.972V11c0 1.25.75 2 2 2 1 0 1 0 1 1v1c0 1-1 2-2 2s-1 .008-1 1.031V20c0 1 0 1 1 1z");
      svg.appendChild(path);
      const path2 = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path2.setAttribute("d", "M15 21c3 0 7-1 7-8V5c0-1.25-.757-2.017-2-2h-4c-1.25 0-2 .75-2 1.972V11c0 1.25.75 2 2 2h.75c0 2.25.25 4-2.75 4v3c0 1 0 1 1 1z");
      svg.appendChild(path2);
      dom.appendChild(svg);

      // No label: a citation whose segment isn't available (not synced yet,
      // or never existed). Keep the node, show it as an unavailable source.
      if (!node.attrs.timestamp) {
        dom.setAttribute("aria-disabled", "true");
        dom.setAttribute("aria-label", "Source unavailable");
        dom.title = "Source unavailable";
        return { dom };
      }

      const text = document.createElement("span");
      text.textContent = node.attrs.timestamp;
      dom.appendChild(text);

      dom.addEventListener("click", (e) => {
        e.preventDefault();
        e.stopPropagation();
        window.dispatchEvent(
          new CustomEvent("yapstack:seek-segment", {
            detail: { offsetSeconds: node.attrs.offsetSeconds },
          }),
        );
      });

      return { dom };
    };
  },

  addCommands() {
    return {
      insertSegmentReference:
        (attrs) =>
        ({ chain }) => {
          return chain()
            .focus()
            .insertContent({
              type: this.name,
              attrs,
            })
            .run();
        },
    };
  },
});
