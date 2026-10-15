# Security policy

## Reporting a vulnerability

Report a vulnerability privately, in one of two ways:

- through GitHub's private vulnerability reporting: the repository's **Security** tab, then **Report a vulnerability** ([direct link](https://github.com/ignaval/twr/security/advisories/new));
- by email to `security@twr.tools`, if you'd rather not use GitHub.

Never report one in a public issue, a pull request or a discussion.

## Supported versions

While twr is before 1.0, only the latest release gets security fixes. A fix ships as a new release; update with `twr update` or the [install script](docs/install.md).

## What happens next

- We acknowledge your report within 3 working days.
- We assess it, and tell you whether we consider it a vulnerability and how severe, within 10 working days.
- We aim to release a fix or a mitigation within 90 days of the report, and keep you informed on the way.
- We disclose it in coordination with you, through a GitHub Security Advisory, with a CVE when one applies.
- We credit you in the advisory, unless you'd rather not be named.

Please give us that time before you disclose anything yourself.

## Scope

[docs/security-model.md](docs/security-model.md) states what twr guarantees and what it doesn't. A report is in scope when it breaks a guarantee twr documents, for example:

- an agent gaining your authority over the hub's socket: answering a permission or question, changing a mode, sending prompts, deleting threads, or emptying or restoring the trash;
- an agent starting a job beyond what the docs allow (another thread's, or in a mode that may not run commands);
- the orchestrator answering a permission, changing a mode or deleting a thread without you, outside the relay rule;
- the orchestrator's project reads escaping their limits (a file outside the project, the work tree, a secret the filters claim to refuse);
- a repository's config making twr's own git run a command, or reach the network through a git transport;
- the notification hook running something its configuration doesn't name;
- a trim or a deletion deleting anything but the build output it names, or the trash deleting outside its own items;
- a runner box authenticating without its token, acting on another box's threads, or getting past the fencing between its connections; the hub listener or the runner accepting plain text where the docs say they don't;
- a client using a socket that belongs to another user;
- `install.sh` or `twr update` installing a release whose signature doesn't verify where the docs say they check it.

Out of scope:

- what a process running as your user can do by design, as [the security model](docs/security-model.md#what-twr-does-not-protect-against) describes: a command an agent has a daemon you run (tmux, `systemd-run --user`, `ssh localhost`, cron) start for it, an agent reading transcripts or twr's files, what a job does once an agent may start it, and the other limits listed there;
- debug builds of twr, which trust a test token by design ([Debug builds](docs/security-model.md#debug-builds));
- the agent CLIs themselves (Claude Code, Codex, Grok Build, ACP agents): their sandboxes, permission modes and behaviour. Report those to their vendors;
- attacks that need control of your user account or write access to your `config.toml`.

If you're not sure whether something is in scope, report it privately anyway.

## What to include

- the twr version (`twr --version`), your OS and architecture, and the runtime and its version if one is involved;
- what an attacker needs (an agent in a given mode, a repository they control, a process on the machine) and what they gain;
- steps to reproduce, ideally a script or a repository, and what you expected instead;
- any logs that help, with secrets removed: `hub.log`, transcripts and `config.toml` can hold tokens, prompts and paths.
