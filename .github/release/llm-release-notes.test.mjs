import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildPrompt,
  extractCommitHashes,
  extractRepoUrl,
  findNewestSection,
  replaceSectionBody,
  validateReply,
} from "./llm-release-notes.mjs";

const REPO = "https://github.com/propeller-heads/fynd";
const A = "a".repeat(40);
const B = "b".repeat(40);
const link = (hash) => `([${hash.slice(0, 7)}](${REPO}/commit/${hash}))`;
const NEWEST_HEADER = `## [1.1.0](${REPO}/compare/1.0.0...1.1.0) (2026-11-02)`;
const NEWEST = `${NEWEST_HEADER}\n\n### Features\n\n* x ${link(A)}\n`;
const OLDER = `## [1.0.0] (2026-10-01)\n\n### Bug Fixes\n\n* y ${link(B)}\n`;

test("finds the newest section", () => {
  const cases = [
    { name: "only section", changelog: NEWEST, text: NEWEST.trimEnd() },
    { name: "followed by another", changelog: `\n\n${NEWEST}\n\n\n${OLDER}`, text: NEWEST },
    { name: "header without link", changelog: OLDER, text: OLDER.trimEnd() },
  ];
  for (const { name, changelog, text } of cases) {
    assert.equal(findNewestSection(changelog).text.trimEnd(), text.trimEnd(), name);
  }
});

test("a changelog without a section is an error", () => {
  assert.throws(() => findNewestSection("# Changelog\n\nnothing\n"), /No '## ' version section/);
});

test("extracts hashes in order without duplicates", () => {
  const text = `* a ${link(B)}\n* b ${link(A)}\n* c ${link(B)}`;
  assert.deepEqual(extractCommitHashes(text), [B, A]);
  assert.deepEqual(extractCommitHashes("## [1.0.0] (2026-10-01)\n\nno commits"), []);
  assert.equal(extractRepoUrl(text), REPO);
});

test("the prompt carries subject, indented body and verbatim commit link", () => {
  const prompt = buildPrompt([{ hash: A, subject: "feat: x", body: "WHY:\nbecause\n" }], REPO);
  assert.ok(prompt.includes(`- feat: x ${link(A)}\n  WHY:\n  because`));
});

test("replacing the body keeps header, spacing and later sections byte-identical", () => {
  const spacing = "\n\n\n\n";
  const changelog = `\n\n${NEWEST.trimEnd()}${spacing}${OLDER}`;
  const result = replaceSectionBody(changelog, `\n### Other\n\n* z ${link(A)}\n\n`);
  assert.equal(result, `\n\n${NEWEST_HEADER}\n\n### Other\n\n* z ${link(A)}${spacing}${OLDER}`);
});

test("replacing the only section ends the file as before", () => {
  const result = replaceSectionBody(OLDER, `* new ${link(B)}`);
  assert.equal(result, `## [1.0.0] (2026-10-01)\n\n* new ${link(B)}\n`);
});

test("rejects replies without commit links or with a version header", () => {
  assert.equal(validateReply(`  * ok ${link(A)}\n`, [A, B]), `* ok ${link(A)}`);
  assert.throws(() => validateReply("* no links", [A]), /none of the commit links/);
  assert.throws(() => validateReply(`## 1.0.0\n* x ${link(A)}`, [A]), /version header/);
});
