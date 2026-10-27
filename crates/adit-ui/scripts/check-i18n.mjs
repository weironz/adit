// Run with `node crates/adit-ui/scripts/check-i18n.mjs [--inventory]`.
// This is a lexical audit, not a Rust parser: comments and cfg(test) modules
// are excluded, while indirect label helpers still need human review.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const han = /\p{Script=Han}/u;
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../src");
export const normalizeTemplate = value => value.replace(/\{(?:[A-Za-z_][A-Za-z_0-9]*|\d*)?(?::[^{}]*)?\}/g, "{}");
const placeholders = value => normalizeTemplate(value).split("{}").length - 1;

export function lex(source) {
  const tokens = [];
  let i = 0;
  while (i < source.length) {
    const start = i;
    if (/\s/.test(source[i])) { i++; continue; }
    if (source.startsWith("//", i)) {
      const end = source.indexOf("\n", i);
      i = end < 0 ? source.length : end;
      continue;
    }
    if (source.startsWith("/*", i)) {
      i += 2;
      let depth = 1;
      while (i < source.length && depth) {
        if (source.startsWith("/*", i)) { depth++; i += 2; }
        else if (source.startsWith("*/", i)) { depth--; i += 2; }
        else i++;
      }
      continue;
    }
    const raw = /^(?:br|r)(#*)"/.exec(source.slice(i));
    if (raw) {
      i += raw[0].length;
      const end = source.indexOf(`"${raw[1]}`, i);
      if (end < 0) throw new Error(`Unterminated raw string at ${start}`);
      tokens.push({ kind: "string", value: source.slice(i, end), start, end: end + 1 + raw[1].length });
      i = end + 1 + raw[1].length;
      continue;
    }
    if (source[i] === '"') {
      i++;
      let value = "";
      while (i < source.length && source[i] !== '"') {
        if (source[i] !== "\\") { value += source[i++]; continue; }
        i++;
        const escape = source[i++];
        const simple = { n: "\n", r: "\r", t: "\t", "0": "\0", '"': '"', "\\": "\\", "'": "'" };
        if (escape in simple) value += simple[escape];
        else if (escape === "u") {
          const end = source.indexOf("}", i);
          value += String.fromCodePoint(parseInt(source.slice(i + 1, end).replaceAll("_", ""), 16));
          i = end + 1;
        } else if (escape === "x") {
          value += String.fromCharCode(parseInt(source.slice(i, i + 2), 16)); i += 2;
        } else if (escape === "\n" || escape === "\r") {
          while (/\s/.test(source[i] ?? "") && i < source.length) i++;
        } else throw new Error(`Unknown string escape at ${i - 1}`);
      }
      if (source[i] !== '"') throw new Error(`Unterminated string at ${start}`);
      tokens.push({ kind: "string", value, start, end: ++i });
      continue;
    }
    // Character literals may contain quotes/braces; lifetimes must remain tokens.
    const character = /^'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\r\n])'/u.exec(source.slice(i));
    if (character) { i += character[0].length; continue; }
    const word = /^[A-Za-z_][A-Za-z_0-9]*/.exec(source.slice(i));
    if (word) {
      i += word[0].length;
      tokens.push({ kind: "word", value: word[0], start, end: i });
    } else {
      tokens.push({ kind: "punct", value: source[i++], start, end: i });
    }
  }
  return tokens;
}

export function productionTokens(source) {
  const tokens = lex(source);
  const out = [];
  for (let i = 0; i < tokens.length; i++) {
    const attr = tokens.slice(i, i + 7).map(t => t.value).join(" ");
    if (attr === "# [ cfg ( test ) ]") {
      let end = i + 7;
      while (end < tokens.length && !["{", ";"].includes(tokens[end].value)) end++;
      if (tokens[end]?.value === "{") {
        let depth = 1;
        while (++end < tokens.length && depth) {
          if (tokens[end].kind === "punct" && tokens[end].value === "{") depth++;
          else if (tokens[end].kind === "punct" && tokens[end].value === "}") depth--;
        }
      } else end++;
      i = end - 1;
    } else out.push(tokens[i]);
  }
  return out;
}

export function catalogEntries(source) {
  const tokens = productionTokens(source);
  const entries = [];
  let table = "";
  for (let i = 0; i < tokens.length; i++) {
    if (tokens[i].value === "const" && ["COMMON", "CONTEXTUAL", "RUNTIME"].includes(tokens[i + 1]?.value)) {
      table = tokens[i + 1].value;
    }
    if (tokens[i].value === ";" && table) table = "";
    if (table && tokens[i].value === "(" && tokens[i + 1]?.kind === "string") {
      const values = [];
      let j = i + 1;
      while (tokens[j]?.kind === "string" || tokens[j]?.value === ",") {
        if (tokens[j].kind === "string") values.push(tokens[j].value);
        j++;
      }
      if (tokens[j]?.value === ")" && values.length === (table === "CONTEXTUAL" ? 3 : 2)) {
        entries.push({ context: table === "CONTEXTUAL" ? values[0] : "", zh: values.at(-2), en: values.at(-1), table });
      }
    }
  }
  return entries;
}

function files(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap(entry => {
    const file = path.join(dir, entry.name);
    return entry.isDirectory() ? files(file) : file.endsWith(".rs") ? [file] : [];
  });
}

export function audit(sourceRoot = root) {
  const catalog = catalogEntries(fs.readFileSync(path.join(sourceRoot, "i18n.rs"), "utf8"));
  const translated = new Set(catalog.map(e => normalizeTemplate(e.zh)));
  const common = new Set(catalog.filter(e => e.table === "COMMON").map(e => normalizeTemplate(e.zh)));
  const contextual = new Set(catalog.filter(e => e.table === "CONTEXTUAL").map(e => `${e.context}:${normalizeTemplate(e.zh)}`));
  const problems = [];
  const seen = new Set();
  for (const entry of catalog) {
    const key = `${entry.table}:${entry.context}:${normalizeTemplate(entry.zh)}`;
    if (seen.has(key)) problems.push(`duplicate: ${key}`);
    seen.add(key);
    if (placeholders(entry.zh) !== placeholders(entry.en)) problems.push(`placeholder count: ${entry.zh}`);
    if (!entry.en || han.test(entry.en)) problems.push(`invalid English: ${entry.zh}`);
  }
  const inventory = [];
  for (const file of files(sourceRoot)) {
    const relative = path.relative(sourceRoot, file).replaceAll("\\", "/");
    if (relative === "i18n.rs" || relative.startsWith("i18n/")) continue;
    // These opt-in acceptance harnesses contain Chinese test data, not product UI.
    if (/acceptance|_tests\.rs$/.test(relative)) continue;
    const source = fs.readFileSync(file, "utf8");
    const tokens = productionTokens(source);
    for (let i = 0; i < tokens.length; i++) {
      const token = tokens[i];
      if (token.kind !== "string" || !han.test(token.value)) continue;
      // This is intentional bilingual font-preview content, not a UI label.
      if (relative === "appearance_editor.rs"
        && token.value === "adit@host:~$  AaBbCc 0123 中文") continue;
      const line = source.slice(0, token.start).split("\n").length;
      const location = `${relative}:${line}`;
      const before = tokens.slice(Math.max(0, i - 8), i).map(t => t.value).join(" ");
      const translatedCall = /\b(t|tf|t_in|tf_in)\s*\($/.test(before)
        || tokens.slice(Math.max(0, i - 5), i).some(t => ["t_in", "tf_in"].includes(t.value));
      const directSink = /(?:\b(?:text|button|text_input|label|set_title|add_filter|String\s*:\s*:\s*from|Err|ok_or)|format !)\s*\($/.test(before);
      const normalized = normalizeTemplate(token.value);
      let covered = translated.has(normalized);
      if (tokens[i - 1]?.value === "(" && ["t", "tf"].includes(tokens[i - 2]?.value)) {
        covered = common.has(normalized);
      } else if (tokens[i - 1]?.value === "," && tokens[i - 2]?.kind === "string"
        && ["t_in", "tf_in"].includes(tokens[i - 4]?.value)) {
        covered = common.has(normalized) || contextual.has(`${tokens[i - 2].value}:${normalized}`);
      }
      inventory.push({ location, value: token.value, catalog: covered, direct: directSink && !translatedCall });
    }
  }
  return { catalog, inventory, problems };
}

// The engine owns these diagnostics, but translations belong only to the GUI.
// This list also makes a merge adding a new SessionOptions/RDP/trigger message
// fail the audit without building either Rust workspace.
const producers = [
  "adit-domain/src/lib.rs", "adit-domain/src/session_options.rs",
  "adit-session/src/lib.rs", "adit-session/src/rdp_client.rs",
  "adit-session/src/terminal_encoding.rs", "adit-session/src/triggers.rs",
  "adit-ssh/src/sftp_transfer.rs", "adit-session/src/sftp_transfers.rs",
  "adit-storage/src/catalog_transfer.rs", "adit-storage/src/ssh_config.rs",
  "adit-storage/src/putty.rs", "adit-storage/src/credentials.rs",
];
const producerExclusions = new Map([
  ["adit-session/src/lib.rs", new Map([
    ["{} 副本", "Generated profile name, stored user data rather than UI chrome"],
    ["正在连接 SFTP: {endpoint}\r\n", "Engine terminal transcript; the GUI never rewrites terminal bytes"],
  ])],
  ["adit-storage/src/catalog_transfer.rs", new Map([
    ["{}：{label}包含控制字符", "Expanded into five specific field-label templates in RUNTIME"],
  ])],
]);

export function auditProducers(catalog, sourceRoot = root) {
  const translated = new Set(catalog.map(entry => normalizeTemplate(entry.zh)));
  const inventory = [];
  for (const relative of producers) {
    const file = path.resolve(sourceRoot, "../../", relative);
    if (!fs.existsSync(file)) continue; // Allows isolated lexer fixtures.
    const source = fs.readFileSync(file, "utf8");
    for (const token of productionTokens(source)) {
      if (token.kind !== "string" || !han.test(token.value)) continue;
      if (producerExclusions.get(relative)?.has(token.value)) continue;
      inventory.push({
        location: `${relative}:${source.slice(0, token.start).split("\n").length}`,
        value: token.value,
        catalog: translated.has(normalizeTemplate(token.value)),
        direct: false,
      });
    }
  }
  return inventory;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const result = audit();
  result.inventory.push(...auditProducers(result.catalog));
  if (process.argv.includes("--inventory")) {
    console.log(JSON.stringify(result.inventory, null, 2));
  } else {
    for (const problem of result.problems) console.error(problem);
    for (const item of result.inventory) {
      if (!item.catalog || item.direct) console.error(`${item.location}: ${!item.catalog ? "missing" : "bypasses lookup"} ${JSON.stringify(item.value)}`);
    }
    const missing = result.inventory.filter(item => !item.catalog || item.direct).length;
    console.log(`${result.catalog.length} catalog entries; ${result.inventory.length} source literals; ${missing} require review; ${result.problems.length} catalog errors`);
    if (missing || result.problems.length) process.exitCode = 1;
  }
}
