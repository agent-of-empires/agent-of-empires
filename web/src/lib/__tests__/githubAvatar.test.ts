import { expect, it } from "vitest";
import { githubAvatar } from "../githubAvatar";

it("builds an avatar URL only for GitHub remote owner keys", () => {
  const octocat = { url: "https://github.com/octocat.png?size=32", owner: "octocat" };
  const cases: Array<[string | null, typeof octocat | null]> = [
    ["octocat@github.com", octocat],
    // `split_host_and_path` keeps the port for ssh:// and https:// remotes.
    ["octocat@GitHub.com:443", octocat],
    ["octocat@ssh.github.com", octocat],
    ["acme@gitlab.com", null],
    // GitHub Enterprise, self-hosted and SSH config aliases are not github.com.
    ["acme@github.example.com", null],
    ["acme@github-work", null],
    ["octocat", null],
    [null, null],
  ];
  for (const [key, want] of cases) {
    expect(githubAvatar(key, 16), String(key)).toEqual(want);
  }
});
