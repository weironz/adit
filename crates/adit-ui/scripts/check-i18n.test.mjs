import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { audit, auditProducers, catalogEntries, lex, normalizeTemplate, productionTokens } from "./check-i18n.mjs";

test("Rust lexer distinguishes comments, character literals, raw and continued strings", () => {
  const source = `// "comment"
  /* "outer" /* "inner" */ */
  let quote = '"'; let lifetime: &'a str = "line\\n";
  let raw = r##"raw "text" // not a comment"##;
  let continued = "first\\
      second"; let unicode = "\\u{4e2d}\\x21";
  `;
  assert.deepEqual(lex(source).filter(t => t.kind === "string").map(t => t.value),
    ["line\n", 'raw "text" // not a comment', "firstsecond", "\u4e2d!"]);
});

test("test-module exclusion ends at its matching brace", () => {
  const source = `fn before() { text("before"); }
    #[cfg(test)] mod tests { fn nested() { let x = r#"{"#; text("test"); } }
    fn after() { text("after"); }`;
  assert.deepEqual(productionTokens(source).filter(t => t.kind === "string").map(t => t.value), ["before", "after"]);
});

test("audit catches a translated entry bypassing lookup, missing text and broken templates", () => {
  const fixture = fs.mkdtempSync(path.join(path.dirname(fileURLToPath(import.meta.url)), ".i18n-audit-"));
  try {
    fs.writeFileSync(path.join(fixture, "i18n.rs"), `
      const COMMON: &[(&str, &str)] = &[("\\u{4e3b}\\u{673a}", "Hosts"), ("{}", "broken")];
      const CONTEXTUAL: &[(&str, &str, &str)] = &[("field", "\\u{4e3b}\\u{673a}", "Host"), ("field", "\\u{4e13}\\u{7528}", "Scoped")];
      const RUNTIME: &[(&str, &str)] = &[("\\u{9519}\\u{8bef}: {error}", "Error: {}")];
    `);
    fs.writeFileSync(path.join(fixture, "view.rs"), 'fn view() { text("\u4e3b\u673a"); String::from("\u4e3b\u673a"); text(t("\u7f3a\u6f0f")); text(t_in("field", "\u4e3b\u673a")); text(t("\u4e13\u7528")); text(t_in("field", "\u4e13\u7528")); text(t_in("other", "\u4e13\u7528")); }');
    const result = audit(fixture);
    assert.equal(result.catalog.length, 5);
    assert.equal(result.inventory.filter(x => x.direct).length, 2);
    assert.equal(result.inventory.filter(x => !x.catalog).length, 3);
    assert.equal(result.problems.length, 1);
    assert.match(result.problems[0], /placeholder count/);
  } finally {
    fs.rmSync(fixture, { recursive: true });
  }
});

test("named engine templates have the same positional contract as UI templates", () => {
  assert.equal(normalizeTemplate("{title}: {error}, {:.1}, {:?}, {}"), "{}: {}, {}, {}, {}");
  assert.equal(catalogEntries('const RUNTIME: &[(&str, &str)] = &[("a {error}", "a {}")];').length, 1);
});

test("product UI and monitored engine producers have complete catalogs and no direct bypasses", () => {
  const result = audit();
  assert.deepEqual(result.problems, []);
  const missing = [...result.inventory, ...auditProducers(result.catalog)].filter(item => !item.catalog || item.direct);
  assert.deepEqual(missing, []);
});
