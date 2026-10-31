import { describe, expect, it } from "vitest";

// Every window's page declares an empty icon. Without one, WebView2 asks
// for /favicon.ico, which Grape doesn't ship, and Tauri logs "asset not
// found" as an error each time a page loads (CI's launch test fails on
// any error line). `data:` is allowed by the CSP's img-src.

const pages = import.meta.glob<string>("../*.html", {
  query: "?raw",
  import: "default",
  eager: true,
});

describe("the window pages", () => {
  it("are the five windows' pages", () => {
    expect(Object.keys(pages).sort()).toEqual([
      "../float.html",
      "../overlay.html",
      "../panel.html",
      "../settings.html",
      "../toast.html",
    ]);
  });

  it.each(Object.entries(pages))(
    "%s declares an empty icon in its head",
    (_, html) => {
      const head = html.slice(0, html.indexOf("</head>"));
      expect(head).toContain('<link rel="icon" href="data:," />');
    },
  );
});
