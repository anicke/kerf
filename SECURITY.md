# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub:
**Security → Report a vulnerability** on
[this repository](https://github.com/anicke/kerf/security/advisories/new).
Do not open a public issue for them.

Once a fix is ready, the advisory is published with credit to you, unless you'd rather stay
anonymous.

## What is in scope

Reports are especially welcome for:

- **The MCP servers.** `kerf-mcp` must not read or write files outside its `--allow` folders,
  and the app's assistant connection (*Preferences → Assistant*) must only accept local programs
  holding the token from the user's own connection file.
- **Opening untrusted files.** A crafted audio file or `.kerf` project should at worst fail to
  open: no crash that could run code, no reading or writing files elsewhere, and no unbounded
  memory or disk use.
- **Exports** writing outside the folder or file you chose, or replacing files without asking.
- **The CI and release workflows**, and anything that could make a build differ from this
  repository's source.

Crashes on malformed media are bugs too; if they don't have a security impact, a normal issue is
fine.

## Supported versions

Kerf has no releases yet. Fixes go to the `main` branch.
