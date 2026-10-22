import { describe, expect, it } from "vitest";

// The UI rule (apps/desktop/ui/CLAUDE.md): the `any` type never appears in
// our TypeScript. This test reads every .ts source under src/ plus the Vite
// config, blanks out comments and string literals, and fails if the word
// `any` is left anywhere in the code. Matching the bare word catches every
// way of writing the type (annotations, assertions, aliases, unions,
// defaults, predicates, `keyof any`, type arguments split over lines)
// without parsing TypeScript. The price: `any` can't be a variable name
// either, and code inside a template literal's `${…}` isn't checked. The
// test skips itself, because its samples must contain the word.

const SELF = "./no-any.test.ts";

const sources = import.meta.glob<string>(["./**/*.ts", "../*.ts"], {
  query: "?raw",
  import: "default",
  eager: true,
});

/**
 * Comments, quoted strings and template literals. One regex, scanned left
 * to right, so a `//` inside a string or a quote inside a comment is read
 * as part of the text it sits in.
 */
const NOT_CODE =
  /\/\*[\s\S]*?\*\/|\/\/[^\n]*|"(?:[^"\\\n]|\\[\s\S])*"|'(?:[^'\\\n]|\\[\s\S])*'|`(?:[^`\\]|\\[\s\S])*`/g;

/** The word `any`, unless it's a property name such as `list.any`. */
const ANY_WORD = /(?<![\w$.])any(?![\w$])/;

/** Blanks out comments and literals, keeping line breaks so line numbers still match. */
function codeOnly(source: string): string {
  return source.replace(NOT_CODE, (text) => text.replace(/[^\n]/g, " "));
}

/** Returns "line: text" for every line whose code uses the word `any`. */
function findAny(source: string): string[] {
  const lines = source.split("\n");
  const hits: string[] = [];
  codeOnly(source)
    .split("\n")
    .forEach((code, index) => {
      if (ANY_WORD.test(code)) {
        hits.push(`${index + 1}: ${(lines[index] ?? "").trim()}`);
      }
    });
  return hits;
}

describe("findAny", () => {
  it("flags every form of the any type", () => {
    const samples = [
      "let x: any = 1;",
      "function f(a: any) {}",
      "const y = z as any;",
      "const w = <any>v;",
      "const m: Map<any, number> = new Map();",
      "type R = Record<string, any>;",
      "const list: string | any[] = [];",
      "const items: readonly any[] = [];",
      "type U = string | any;",
      "type V = any | string;",
      "type Payload = any;",
      "function g<T = any>(t: T): T { return t; }",
      "function h(x: unknown): x is any { return true; }",
      "type K = keyof any;",
      'const s = "//"; let y: any;',
      "const t = 'it\\'s'; let z: any;",
    ];
    for (const sample of samples) {
      expect(findAny(sample), sample).toHaveLength(1);
    }
  });

  it("flags a type argument on its own line", () => {
    expect(findAny("const m: Map<string,\n  any> = new Map();")).toEqual([
      "2: any> = new Map();",
    ]);
  });

  it("ignores the word in comments, strings and names", () => {
    const samples = [
      "// Hide the wheel if anything goes wrong.",
      "// Drop any file here.",
      "/* any */ const a = 1;",
      "/*\n * Pick any segment.\n */",
      'const label = "Pick any segment";',
      "const label = 'any';",
      "const label = `any ${count} files`;",
      "const company = anyway(many);",
      "const found = list.any;",
      "type T = unknown;",
    ];
    for (const sample of samples) {
      expect(findAny(sample), sample).toEqual([]);
    }
  });
});

describe("UI sources", () => {
  const files = Object.entries(sources).filter(([path]) => path !== SELF);

  it("include the sources this test is meant to scan", () => {
    const paths = files.map(([path]) => path);
    expect(paths).toContain("./shared/dom.ts");
    expect(paths).toContain("../vite.config.ts");
  });

  it.each(files)("%s never uses the any type", (_path, source) => {
    expect(findAny(source)).toEqual([]);
  });
});
