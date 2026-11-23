/**
 * Opt-in jsdom layout for virtualized lists. jsdom does no layout, so every
 * size reads 0 and a virtualizer renders nothing. This gives the scroll
 * viewport, its rows and its size container just enough geometry for
 * `@tanstack/virtual-core`:
 *
 * - the viewport (`[data-slot='scroll-area-viewport']`) has a fixed
 *   `clientWidth`/`clientHeight`/`offsetHeight`; its `scrollHeight` is the
 *   tallest inline `height` inside it (the virtualizer's size container);
 * - a row (`[data-index]`) is `heightOf(row)` tall, positioned by its inline
 *   `translateY`;
 * - `scrollTop` is stored and clamped, and every change dispatches a `scroll`
 *   event on a later task, as a browser does;
 * - `scrollTo`, `scrollIntoView` (`nearest`/`center`/`start`/`end`) and
 *   `getBoundingClientRect` follow from those numbers;
 * - a `ResizeObserver` whose `resize()` re-delivers every observed row's
 *   current height, for tests that change a row's size after it mounted.
 *
 * Install in `beforeEach` and call `uninstall()` in `afterEach`.
 */

const VIEWPORT_SELECTOR = "[data-slot='scroll-area-viewport']";

export interface VirtualLayoutOptions {
  viewportHeight?: number;
  viewportWidth?: number;
  /** Height of a mounted `[data-index]` row. */
  heightOf?: (row: HTMLElement) => number;
}

export interface VirtualLayout {
  viewport(): HTMLElement;
  /** Sets `scrollTop` as a user scroll would, then lets the scroll event and React settle. */
  scrollTo(top: number): Promise<void>;
  /** Re-delivers row sizes to the virtualizer's ResizeObserver. */
  resize(): void;
  /** Lets pending scroll events, timers and animation frames run. */
  settle(ms?: number): Promise<void>;
  /** `[data-index]` rows currently in the DOM, by index. */
  mountedIndexes(): number[];
  uninstall(): void;
}

function isViewport(el: Element): boolean {
  return el.matches(VIEWPORT_SELECTOR);
}

function translateY(el: HTMLElement): number {
  const m = /translateY\((-?[\d.]+)px\)/.exec(el.style.transform);
  return m ? Number(m[1]) : 0;
}

function inlineHeight(el: HTMLElement): number | null {
  return el.style.height.endsWith("px") ? parseFloat(el.style.height) : null;
}

export function installVirtualLayout({
  viewportHeight = 600,
  viewportWidth = 400,
  heightOf = () => 50,
}: VirtualLayoutOptions = {}): VirtualLayout {
  const proto = HTMLElement.prototype;
  const elProto = Element.prototype;
  const saved = new Map<string, PropertyDescriptor | undefined>();
  const save = (target: object, name: string) =>
    saved.set(
      `${target === proto ? "h" : "e"}:${name}`,
      Object.getOwnPropertyDescriptor(target, name),
    );
  const savedRO = window.ResizeObserver;

  const scrollTops = new WeakMap<Element, number>();
  const pendingScroll = new Set<Element>();

  const contentHeight = (viewport: Element) => {
    let max = 0;
    viewport.querySelectorAll<HTMLElement>("[style*='height']").forEach((el) => {
      max = Math.max(max, inlineHeight(el) ?? 0);
    });
    return max;
  };
  const maxScrollTop = (viewport: Element) =>
    Math.max(0, contentHeight(viewport) - viewportHeight);

  const rowHeight = (el: HTMLElement) => heightOf(el);

  const writeScrollTop = (el: Element, value: number) => {
    const next = isViewport(el)
      ? Math.min(Math.max(0, value), maxScrollTop(el))
      : 0;
    if ((scrollTops.get(el) ?? 0) === next) return;
    scrollTops.set(el, next);
    if (pendingScroll.has(el)) return;
    pendingScroll.add(el);
    setTimeout(() => {
      pendingScroll.delete(el);
      el.dispatchEvent(new Event("scroll"));
    }, 0);
  };

  const viewportOf = (el: Element) => el.closest(VIEWPORT_SELECTOR);

  const define = (target: object, name: string, desc: PropertyDescriptor) => {
    save(target, name);
    Object.defineProperty(target, name, { configurable: true, ...desc });
  };

  define(proto, "offsetHeight", {
    get(this: HTMLElement) {
      if (isViewport(this)) return viewportHeight;
      if (this.hasAttribute("data-index")) return rowHeight(this);
      return inlineHeight(this) ?? 0;
    },
  });
  define(proto, "offsetWidth", {
    get(this: HTMLElement) {
      return viewportOf(this) ? viewportWidth : 0;
    },
  });
  define(elProto, "clientHeight", {
    get(this: Element) {
      return isViewport(this) ? viewportHeight : 0;
    },
  });
  define(elProto, "clientWidth", {
    get(this: Element) {
      return isViewport(this) ? viewportWidth : 0;
    },
  });
  define(elProto, "scrollHeight", {
    get(this: Element) {
      return isViewport(this)
        ? Math.max(viewportHeight, contentHeight(this))
        : 0;
    },
  });
  define(elProto, "scrollTop", {
    get(this: Element) {
      return scrollTops.get(this) ?? 0;
    },
    set(this: Element, value: number) {
      writeScrollTop(this, value);
    },
  });
  define(elProto, "scrollTo", {
    value(this: Element, arg: ScrollToOptions | number, y?: number) {
      const top = typeof arg === "number" ? y : arg.top;
      if (top != null) writeScrollTop(this, top);
    },
  });

  // Content-space box of `el`: its row's, or the whole content for the
  // size container and anything else inside the viewport.
  const contentBox = (el: Element): { top: number; height: number } | null => {
    const row = el.closest<HTMLElement>("[data-index]");
    if (row) return { top: translateY(row), height: rowHeight(row) };
    const viewport = viewportOf(el);
    if (viewport && viewport !== el) {
      return { top: 0, height: contentHeight(viewport) };
    }
    return null;
  };

  define(elProto, "getBoundingClientRect", {
    value(this: Element) {
      let top = 0;
      let height = 0;
      if (isViewport(this)) {
        height = viewportHeight;
      } else {
        const box = contentBox(this);
        const viewport = viewportOf(this);
        if (box && viewport) {
          top = box.top - (scrollTops.get(viewport) ?? 0);
          height = box.height;
        }
      }
      const width = viewportOf(this) || isViewport(this) ? viewportWidth : 0;
      return {
        top,
        bottom: top + height,
        left: 0,
        right: width,
        width,
        height,
        x: 0,
        y: top,
        toJSON() {},
      } as DOMRect;
    },
  });
  define(elProto, "scrollIntoView", {
    value(this: Element, arg?: boolean | ScrollIntoViewOptions) {
      const viewport = viewportOf(this);
      const box = contentBox(this);
      if (!viewport || !box) return;
      const block = typeof arg === "object" ? (arg.block ?? "start") : "start";
      const current = scrollTops.get(viewport) ?? 0;
      let next = current;
      if (block === "start") next = box.top;
      else if (block === "end") next = box.top + box.height - viewportHeight;
      else if (block === "center") {
        next = box.top + box.height / 2 - viewportHeight / 2;
      } else if (box.top < current) next = box.top;
      else if (box.top + box.height > current + viewportHeight) {
        next = box.top + box.height - viewportHeight;
      }
      writeScrollTop(viewport, next);
    },
  });

  const observers = new Set<{
    cb: ResizeObserverCallback;
    targets: Set<Element>;
    self: ResizeObserver;
  }>();
  class LayoutResizeObserver {
    private entry: { cb: ResizeObserverCallback; targets: Set<Element>; self: ResizeObserver };
    constructor(cb: ResizeObserverCallback) {
      this.entry = { cb, targets: new Set(), self: this as unknown as ResizeObserver };
      observers.add(this.entry);
    }
    observe(target: Element) {
      this.entry.targets.add(target);
    }
    unobserve(target: Element) {
      this.entry.targets.delete(target);
    }
    disconnect() {
      this.entry.targets.clear();
      observers.delete(this.entry);
    }
  }
  window.ResizeObserver = LayoutResizeObserver as unknown as typeof ResizeObserver;

  const getViewport = () => {
    const el = document.querySelector<HTMLElement>(VIEWPORT_SELECTOR);
    if (!el) throw new Error("no scroll viewport mounted");
    return el;
  };

  const settle = async (ms = 50) => {
    const { act } = await import("@testing-library/react");
    await act(() => new Promise((r) => setTimeout(r, ms)));
  };

  return {
    viewport: getViewport,
    async scrollTo(top) {
      getViewport().scrollTop = top;
      await settle();
    },
    resize() {
      for (const { cb, targets, self } of [...observers]) {
        const entries = [...targets]
          .filter((t) => t.hasAttribute("data-index"))
          .map((target) => {
            const blockSize = rowHeight(target as HTMLElement);
            return {
              target,
              borderBoxSize: [{ blockSize, inlineSize: viewportWidth }],
              contentBoxSize: [{ blockSize, inlineSize: viewportWidth }],
              contentRect: { height: blockSize, width: viewportWidth },
            } as unknown as ResizeObserverEntry;
          });
        if (entries.length > 0) cb(entries, self);
      }
    },
    settle,
    mountedIndexes() {
      return [...document.querySelectorAll("[data-index]")]
        .map((el) => Number(el.getAttribute("data-index")))
        .sort((a, b) => a - b);
    },
    uninstall() {
      for (const [k, desc] of saved) {
        const target = k.startsWith("h:") ? proto : elProto;
        const name = k.slice(2);
        if (desc) Object.defineProperty(target, name, desc);
        else delete (target as unknown as Record<string, unknown>)[name];
      }
      window.ResizeObserver = savedRO;
    },
  };
}
