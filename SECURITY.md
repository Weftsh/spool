# Security policy

Spool holds an organisation's source code, and its runner agent executes
workflow steps on machines the organisation owns. We treat every report
about access control, credentials or code execution as urgent.

## Reporting a vulnerability

**Do not open a public issue.** Report privately through either:

- GitHub private vulnerability reporting: the **Security** tab of this
  repository, then **Report a vulnerability**; or
- email to **security@weft.sh**.

Please include the affected component and version, the steps to reproduce,
and the impact you observed. You will get an acknowledgement within two
business days and a status update at least weekly until the issue is closed.

## Disclosure

We follow a **90-day coordinated disclosure** window from the date of your
report, shorter if a fix ships sooner or the issue is being exploited. We
credit reporters in the advisory unless you ask us not to.

## What is in scope

- Reading or writing a repository, change, changeset, issue, check or log
  without a role that allows it — over HTTP, SSH or the REST API — including
  learning that one exists.
- Bypassing authentication, session or token scoping: a token bound to one
  repository or one organisation reaching another, a revoked credential
  still working, a viewer performing a write.
- Using a runner or job credential beyond its job: a workflow step reaching
  another repository, another organisation's jobs, or the server's own
  secrets; a job from a fork running before a maintainer approved it.
- Forging webhooks, CI verdicts or GitHub App callbacks the server accepts.
- Corrupting or losing acknowledged writes: a push, ref update or land that
  was reported as accepted and is later missing or altered.
- Tampering with release artifacts.

A workflow step runs as the operating-system user that started
`weft-runner`, on that machine, by design: what a step can do to the machine
it runs on is the operator's to constrain, not a vulnerability.

## Supported versions

Security fixes land on the latest release. Before general availability,
only the latest pre-release is supported.
