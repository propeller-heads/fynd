// Rewrites the newest CHANGELOG.md section (written by release-plz) with an LLM via OpenRouter.
// Run locally: OPENROUTER_API_KEY=... node .github/release/llm-release-notes.mjs [CHANGELOG.md]
// The LLM is best-effort: on any LLM failure the file is left unchanged and the exit code is 0.
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const DEFAULT_MODEL = "anthropic/claude-sonnet-5.5";
const OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions";
const COMMIT_LINK = /\/commit\/([0-9a-f]{40})\b/g;

/** Finds the newest section: from the first `## ` line up to the next one (or EOF). */
export function findNewestSection(changelog) {
  const lines = changelog.split("\n");
  const start = lines.findIndex((line) => line.startsWith("## "));
  if (start === -1) {
    throw new Error("No '## ' version section found in the changelog");
  }
  let end = lines.findIndex((line, i) => i > start && line.startsWith("## "));
  if (end === -1) {
    end = lines.length;
  }
  return { lines, start, end, text: lines.slice(start, end).join("\n") };
}

/** Returns the version in a section header, e.g. `0.118.5` from `## [0.118.5](...) (date)`. */
export function sectionVersion(headerLine) {
  const match = headerLine.match(/\[([^\]]+)\]/);
  if (!match) {
    throw new Error(`No version in changelog header: ${headerLine}`);
  }
  return match[1];
}

/** Returns the full commit hashes linked in the section, in order, without duplicates. */
export function extractCommitHashes(sectionText) {
  const hashes = new Set();
  for (const match of sectionText.matchAll(COMMIT_LINK)) {
    hashes.add(match[1]);
  }
  return [...hashes];
}

/** Returns the repository URL from the first commit link, e.g. https://github.com/org/repo. */
export function extractRepoUrl(sectionText) {
  const match = sectionText.match(/(https:\/\/[^\s()[\]]+?)\/commit\/[0-9a-f]{40}/);
  if (!match) {
    throw new Error("No commit link found to derive the repository URL from");
  }
  return match[1];
}

/** Builds the commit link release-plz uses, e.g. [abc1234](https://.../commit/abc1234...). */
export function commitLink(repoUrl, hash) {
  return `([${hash.slice(0, 7)}](${repoUrl}/commit/${hash}))`;
}

/** Builds the LLM prompt from commits shaped as {hash, subject, body}. */
export function buildPrompt(commits, repoUrl) {
  const entries = commits.map(({ hash, subject, body }) => {
    const indentedBody = body
      .trim()
      .split("\n")
      .filter((line) => line.trim() !== "")
      .map((line) => `  ${line}`)
      .join("\n");
    const head = `- ${subject} ${commitLink(repoUrl, hash)}`;
    return indentedBody ? `${head}\n${indentedBody}` : head;
  });
  return [
    "Write a changelog entry body for a software release.",
    "Group changes under H3 headings (### Features, ### Bug Fixes, ### Performance, ### Other)" +
      " where they apply.",
    "Each bullet: concise, user-facing, imperative. Use the commit bodies to explain what changed" +
      " for users, not to copy them.",
    "Each input commit carries a markdown commit link in parentheses, e.g. ([abc1234](url))." +
      " End every changelog bullet with the link(s) of the commit(s) it summarizes, copied" +
      " verbatim. If a bullet merges several commits, append all of their links separated by" +
      " a space.",
    "Return ONLY the markdown body: no top-level version header, no preamble.",
    "",
    "Commits:",
    ...entries,
  ].join("\n");
}

/** Returns the trimmed reply, or throws if it keeps no commit link or starts a `## ` header. */
export function validateReply(reply, hashes) {
  const text = reply.trim();
  if (/^## /m.test(text)) {
    throw new Error("reply contains a '## ' version header");
  }
  if (!hashes.some((hash) => text.includes(`/commit/${hash}`))) {
    throw new Error("reply keeps none of the commit links");
  }
  return text;
}

/** Replaces the newest section body, keeping its header, trailing blank lines and the rest. */
export function replaceSectionBody(changelog, newBody) {
  const { lines, start, end } = findNewestSection(changelog);
  let trailing = end;
  while (trailing > start + 1 && lines[trailing - 1].trim() === "") {
    trailing -= 1;
  }
  const blanks = lines.slice(trailing, end);
  if (blanks.length === 0 && end < lines.length) {
    blanks.push("");
  }
  return [
    ...lines.slice(0, start + 1),
    "",
    ...newBody.trim().split("\n"),
    ...blanks,
    ...lines.slice(end),
  ].join("\n");
}

function isTagged(version) {
  const out = execFileSync("git", ["tag", "--list", version], { encoding: "utf8" });
  return out.trim() !== "";
}

function readCommit(hash) {
  const out = execFileSync("git", ["show", "-s", "--format=%s%x00%b", hash], {
    encoding: "utf8",
  });
  const [subject, body = ""] = out.split("\0");
  return { hash, subject, body };
}

async function askModel(prompt) {
  const model = process.env.LLM_RELEASE_NOTES_MODEL || DEFAULT_MODEL;
  const response = await fetch(OPENROUTER_URL, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${process.env.OPENROUTER_API_KEY}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      model,
      max_tokens: 4096,
      messages: [{ role: "user", content: prompt }],
    }),
  });
  if (!response.ok) {
    throw new Error(`OpenRouter returned HTTP ${response.status}`);
  }
  const data = await response.json();
  const content = data?.choices?.[0]?.message?.content;
  if (!content) {
    throw new Error("OpenRouter response has no message content");
  }
  return content;
}

function skip(reason) {
  console.log(`::warning::LLM release notes skipped: ${reason}`);
}

export async function main(path = "CHANGELOG.md") {
  const changelog = readFileSync(path, "utf8");
  const section = findNewestSection(changelog);
  const version = sectionVersion(section.lines[section.start]);
  if (isTagged(version)) {
    console.log(`Newest section ${version} is already released; nothing to rewrite.`);
    return;
  }
  const sectionText = section.text;
  const hashes = extractCommitHashes(sectionText);
  if (hashes.length === 0) {
    console.log("No commit links in the newest section; nothing to rewrite.");
    return;
  }
  if (!process.env.OPENROUTER_API_KEY) {
    skip("OPENROUTER_API_KEY is not set");
    return;
  }
  try {
    const prompt = buildPrompt(hashes.map(readCommit), extractRepoUrl(sectionText));
    const body = validateReply(await askModel(prompt), hashes);
    writeFileSync(path, replaceSectionBody(changelog, body));
    console.log(`Rewrote release notes from ${hashes.length} commits.`);
  } catch (error) {
    skip(error.message);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv[2]).catch((error) => {
    console.error(`error: ${error.message}`);
    process.exit(1);
  });
}
