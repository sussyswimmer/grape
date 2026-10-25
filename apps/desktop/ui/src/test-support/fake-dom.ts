/**
 * A tiny stand-in for the DOM, just big enough for the wheel and toast
 * renderers, the settings page and the float shelf, so their tests run in
 * Node without a new dependency. It models elements, attributes, classes,
 * inline style, children, text, listeners (with bubbling, `target`,
 * `preventDefault` and `stopPropagation`), focus, pointer capture, and a
 * form control's `value` and `checked` (plain fields: a test sets them,
 * then dispatches `change`); nothing else. Test code only.
 */

export class FakeClassList {
  private readonly names = new Set<string>();

  add(...names: string[]): void {
    for (const name of names) this.names.add(name);
  }

  remove(...names: string[]): void {
    for (const name of names) this.names.delete(name);
  }

  contains(name: string): boolean {
    return this.names.has(name);
  }

  toggle(name: string, force?: boolean): boolean {
    const on = force ?? !this.names.has(name);
    if (on) this.names.add(name);
    else this.names.delete(name);
    return on;
  }

  toString(): string {
    return [...this.names].join(" ");
  }
}

export class FakeStyle {
  readonly props = new Map<string, string>();

  setProperty(name: string, value: string): void {
    this.props.set(name, value);
  }

  getPropertyValue(name: string): string {
    return this.props.get(name) ?? "";
  }

  removeProperty(name: string): string {
    const old = this.getPropertyValue(name);
    this.props.delete(name);
    return old;
  }
}

/** What `dispatch` hands every listener: one object for the whole bubble. */
export interface FakeEvent {
  readonly type: string;
  readonly target: FakeElement;
  defaultPrevented: boolean;
  preventDefault(): void;
  stopPropagation(): void;
  readonly [field: string]: unknown;
}

type Listener = (event: FakeEvent) => void;

export class FakeElement {
  readonly attributes = new Map<string, string>();
  readonly classList = new FakeClassList();
  readonly style = new FakeStyle();
  readonly listeners = new Map<string, Listener[]>();
  children: FakeElement[] = [];
  parent: FakeElement | null = null;
  value = "";
  checked = false;
  /** Pointer ids captured with `setPointerCapture` and not yet released. */
  readonly captures = new Set<number>();
  private text = "";

  constructor(
    readonly ownerDocument: FakeDocument,
    readonly namespaceURI: string | null,
    readonly localName: string,
  ) {}

  setAttribute(name: string, value: string): void {
    if (name === "class") {
      this.classList.remove(...this.classList.toString().split(" ").filter(Boolean));
      this.classList.add(...value.split(/\s+/).filter(Boolean));
      return;
    }
    this.attributes.set(name, String(value));
  }

  getAttribute(name: string): string | null {
    if (name === "class") {
      const names = this.classList.toString();
      return names === "" ? null : names;
    }
    return this.attributes.get(name) ?? null;
  }

  removeAttribute(name: string): void {
    this.attributes.delete(name);
  }

  hasAttribute(name: string): boolean {
    return this.getAttribute(name) !== null;
  }

  append(...nodes: FakeElement[]): void {
    for (const node of nodes) {
      node.parent?.removeChild(node);
      node.parent = this;
      this.children.push(node);
    }
    this.text = "";
  }

  removeChild(node: FakeElement): void {
    this.children = this.children.filter((child) => child !== node);
    node.parent = null;
  }

  replaceChildren(...nodes: FakeElement[]): void {
    for (const child of this.children) child.parent = null;
    this.children = [];
    this.text = "";
    this.append(...nodes);
  }

  get textContent(): string {
    return this.children.length > 0
      ? this.children.map((child) => child.textContent).join("")
      : this.text;
  }

  set textContent(value: string) {
    for (const child of this.children) child.parent = null;
    this.children = [];
    this.text = value;
  }

  addEventListener(type: string, listener: Listener): void {
    const list = this.listeners.get(type) ?? [];
    list.push(listener);
    this.listeners.set(type, list);
  }

  focus(): void {
    this.ownerDocument.activeElement = this;
  }

  setPointerCapture(pointerId: number): void {
    this.captures.add(pointerId);
  }

  releasePointerCapture(pointerId: number): void {
    this.captures.delete(pointerId);
  }

  hasPointerCapture(pointerId: number): boolean {
    return this.captures.has(pointerId);
  }

  removeEventListener(type: string, listener: Listener): void {
    const list = this.listeners.get(type) ?? [];
    this.listeners.set(
      type,
      list.filter((item) => item !== listener),
    );
  }

  /**
   * Test helper: fires `type` on this element and every ancestor
   * (bubbling), until a listener calls `stopPropagation`. `fields` go on
   * the event, e.g. `{ key: "Enter" }`. Returns the event, so a test can
   * check `defaultPrevented`.
   */
  dispatch(type: string, fields: Readonly<Record<string, unknown>> = {}): FakeEvent {
    let stopped = false;
    const event: FakeEvent = {
      target: this,
      defaultPrevented: false,
      preventDefault() {
        event.defaultPrevented = true;
      },
      stopPropagation() {
        stopped = true;
      },
      ...fields,
      type,
    };
    let node: FakeElement | null = this;
    while (node !== null && !stopped) {
      for (const listener of node.listeners.get(type) ?? []) listener(event);
      node = node.parent;
    }
    return event;
  }
}

export class FakeDocument {
  readonly defaultView = null;
  activeElement: FakeElement | null = null;

  createElementNS(namespaceURI: string, name: string): FakeElement {
    return new FakeElement(this, namespaceURI, name);
  }

  createElement(name: string): FakeElement {
    return new FakeElement(this, "http://www.w3.org/1999/xhtml", name);
  }
}

/** A fresh document and an empty root element in it. */
export function fakeRoot(): { doc: FakeDocument; root: FakeElement } {
  const doc = new FakeDocument();
  return { doc, root: doc.createElement("div") };
}

/** Every descendant of `element`, depth first. */
export function descendants(element: FakeElement): FakeElement[] {
  return element.children.flatMap((child) => [child, ...descendants(child)]);
}

/** Descendants with this local name. */
export function byTag(element: FakeElement, name: string): FakeElement[] {
  return descendants(element).filter((node) => node.localName === name);
}

/** Descendants with this class. */
export function byClass(element: FakeElement, name: string): FakeElement[] {
  return descendants(element).filter((node) => node.classList.contains(name));
}

/** The only descendant with this id; throws otherwise. */
export function byId(element: FakeElement, id: string): FakeElement {
  const found = descendants(element).filter((node) => node.getAttribute("id") === id);
  const [only] = found;
  if (found.length !== 1 || only === undefined) {
    throw new Error(`expected one #${id}, found ${found.length}`);
  }
  return only;
}

/** The only descendant with this class; throws otherwise. */
export function oneByClass(element: FakeElement, name: string): FakeElement {
  const found = byClass(element, name);
  if (found.length !== 1) {
    throw new Error(`expected one .${name}, found ${found.length}`);
  }
  const [only] = found;
  if (only === undefined) throw new Error(`no .${name}`);
  return only;
}
