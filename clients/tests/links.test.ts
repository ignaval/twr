import { describe, expect, it } from "vitest";
import { externalLink, linkConfirmation, textHost } from "../shared/links";

const confirm = (href: string, text: string, terminal = false) =>
  linkConfirmation({ href, text, terminal });

describe("links that ask before opening", () => {
  it("opens an https link whose text is words or its own address at once", () => {
    expect(confirm("https://docs.example.com/guide", "the guide")).toBe(
      undefined,
    );
    expect(
      confirm("https://docs.example.com/guide", "docs.example.com/guide"),
    ).toBe(undefined);
    expect(
      confirm(
        "https://www.example.com/?q=1",
        "https://example.com/somewhere-else",
      ),
    ).toBe(undefined);
    expect(confirm("mailto:me@example.com", "me@example.com")).toBe(undefined);
  });

  it("asks when the text names another host than the one it opens", () => {
    expect(confirm("https://evil.test/?q=secret", "github.com")).toMatch(
      /names github\.com, but it opens evil\.test/,
    );
    expect(
      confirm("https://evil.test/", "https://github.com/ignaval/twr"),
    ).toMatch(/github\.com/);
    expect(confirm("https://xn--pple-43d.com/", "apple.com")).toMatch(
      /but it opens xn--pple-43d\.com/,
    );
    expect(confirm("mailto:attacker@evil.test", "me@example.com")).toMatch(
      /writes to attacker@evil\.test/,
    );
  });

  it("asks before any plain http link", () => {
    expect(confirm("http://example.com/", "example")).toMatch(/plain http/);
    expect(confirm("http://localhost:3000/", "http://localhost:3000/")).toMatch(
      /plain http/,
    );
  });

  it("asks for every terminal hyperlink whose text differs from its address", () => {
    expect(confirm("https://example.com/", "click here", true)).toMatch(
      /terminal shows different text/,
    );
    expect(confirm("https://example.com/", "https://example.com/", true)).toBe(
      undefined,
    );
    expect(confirm("https://example.com", "https://example.com", true)).toBe(
      undefined,
    );
  });

  it("does not take file names for hosts", () => {
    expect(textHost("App.tsx")).toBeUndefined();
    expect(textHost("README.md")).toBeUndefined();
    expect(textHost("main.rs")).toBeUndefined();
    expect(textHost("see the docs")).toBeUndefined();
    expect(textHost("example.com")).toBe("example.com");
    expect(textHost("www.example.md")).toBe("example.md");
    expect(textHost("docs.rs/serde")).toBe("docs.rs");
    expect(textHost("10.0.0.1:8080")).toBe("10.0.0.1");
    expect(
      confirm("https://github.com/ignaval/twr/blob/dev/README.md", "README.md"),
    ).toBe(undefined);
  });

  it("opens only web and email links; paths and other schemes stay text", () => {
    expect(externalLink("https://example.com")?.hostname).toBe("example.com");
    expect(externalLink("mailto:a@b.c")).toBeDefined();
    expect(externalLink("clients/shared/App.tsx:42")).toBeUndefined();
    expect(externalLink("file:///etc/passwd")).toBeUndefined();
    expect(externalLink("#section")).toBeUndefined();
    expect(externalLink("javascript:alert(1)")).toBeUndefined();
    expect(externalLink(undefined)).toBeUndefined();
    expect(confirm("file:///etc/passwd", "x")).toBeUndefined();
  });
});
