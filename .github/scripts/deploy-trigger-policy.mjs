import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

export const PROTECTED_REF = "refs/heads/main";

/**
 * Fail-closed policy for deciding whether a CI event may reach deploy secrets.
 * The branch object is the current GitHub API response for `branches/main`.
 */
export function evaluateDeployTrigger({
  eventName,
  repository,
  ref,
  sha,
  event,
  branch,
}) {
  const failures = [];

  if (eventName !== "push") failures.push("event is not a push");
  if (ref !== PROTECTED_REF) failures.push("ref is not refs/heads/main");
  if (event?.ref !== PROTECTED_REF) failures.push("event ref is not main");
  if (event?.repository?.full_name !== repository) {
    failures.push("event repository does not match the current repository");
  }
  if (event?.deleted === true) failures.push("push deleted the branch");
  if (!event?.after || event.after !== sha) {
    failures.push("event after SHA does not match the workflow SHA");
  }
  if (branch?.protected !== true) failures.push("main is not protected");
  if (!branch?.commit?.sha || branch.commit.sha !== sha) {
    failures.push("workflow SHA is not the current protected-main head");
  }

  return { trusted: failures.length === 0, failures };
}

async function fetchProtectedMain(repository, token) {
  const response = await fetch(
    `https://api.github.com/repos/${repository}/branches/main`,
    {
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${token}`,
        "X-GitHub-Api-Version": "2022-11-28",
      },
    },
  );

  if (!response.ok) {
    throw new Error(`GitHub branch lookup failed with HTTP ${response.status}`);
  }
  return response.json();
}

async function main() {
  const required = [
    "GITHUB_EVENT_NAME",
    "GITHUB_EVENT_PATH",
    "GITHUB_REPOSITORY",
    "GITHUB_REF",
    "GITHUB_SHA",
    "GITHUB_TOKEN",
  ];
  const missing = required.filter((name) => !process.env[name]);
  if (missing.length > 0) {
    throw new Error(`missing required environment: ${missing.join(", ")}`);
  }

  const event = JSON.parse(
    await readFile(process.env.GITHUB_EVENT_PATH, "utf8"),
  );
  const branch = await fetchProtectedMain(
    process.env.GITHUB_REPOSITORY,
    process.env.GITHUB_TOKEN,
  );
  const result = evaluateDeployTrigger({
    eventName: process.env.GITHUB_EVENT_NAME,
    repository: process.env.GITHUB_REPOSITORY,
    ref: process.env.GITHUB_REF,
    sha: process.env.GITHUB_SHA,
    event,
    branch,
  });

  if (!result.trusted) {
    throw new Error(`deploy denied: ${result.failures.join("; ")}`);
  }

  if (process.env.GITHUB_OUTPUT) {
    const { appendFile } = await import("node:fs/promises");
    await appendFile(process.env.GITHUB_OUTPUT, "trusted=true\n");
  }
  console.log(`deploy authorized for protected main SHA ${process.env.GITHUB_SHA}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  });
}
