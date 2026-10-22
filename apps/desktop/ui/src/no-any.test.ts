import { describe, expect, it } from "vitest";

// The UI rule (apps/desktop/ui/CLAUDE.md): the `any` type never appears in
// our TypeScript. This test reads every .ts source under src/ plus the Vite
// config and fails on an annotation, an assertion or a type argument that
// uses it. It skips itself, because its samples must contain the patterns.

const SELF = "./no-any.test.ts";

const sources = import.meta.glob<string>(["./**/*.ts", "../*.ts"], {
  query: "?raw",
  import: "default",
  eager: true,
});

const PATTERNS: readonly RegExp[] = [
  /:\s*any\b/, // annotation, e.g. `x: any`
  /\bas\s+any\b/, // assertion
  /<\s*any\s*[>,[]/, // type argument or old-style cast
  /,\s*any\s*>/, // later type argument, e.g. Record<string, any>
  /\bany\s*\[\]/, // array of it
  /[|&]\s*any\b/, // union or intersection member
];

/** Returns "line: text" for every line that uses the `any` type. */
function findAny(source: string): string[] {
  const hits: string[] = [];
  source.split("\n").forEach((line, index) => {
    if (PATTERNS.some((pattern) => pattern.test(line))) {
      hits.push(`${index + 1}: ${line.trim()}`);
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
      "type U = string | any;",
    ];
    for (const sample of samples) {
      expect(findAny(sample), sample).toHaveLength(1);
    }
  });

  it("ignores ordinary words", () => {
    const samples = [
      "// Hide the wheel if anything goes wrong.",
      'const label = "Pick any segment";',
      "const company = anyway(many);",
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
