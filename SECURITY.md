# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub private vulnerability reporting:
open the repository's [Security tab](https://github.com/ozten/openboard/security)
and choose "Report a vulnerability", or go straight to
<https://github.com/ozten/openboard/security/advisories/new>.

Private vulnerability reporting is not enabled on the repository yet. It will
be turned on with milestone M2 of the [roadmap](ROADMAP.md). Until then, open
an issue that asks for a private contact and gives no details of the problem.

Do not put the details of a suspected vulnerability in a public issue, pull
request or discussion.

A useful report includes:

- the commit or version you tested;
- how the instance was configured and deployed;
- steps to reproduce, or a proof of concept;
- what an attacker gains.

## Response time

- We acknowledge a report within 7 days.
- After acknowledging it, we tell you whether we accept it as a vulnerability
  and what we plan to do, and we keep you informed until it is resolved.
- We publish a GitHub security advisory when a fix is available and credit the
  reporter unless asked not to.

Please give us a reasonable chance to release a fix before you disclose the
issue publicly.

## Supported versions

Openboard is pre-release. Only the current `main` branch receives security
fixes. No tagged release is supported yet; this section will be updated when
the first release is made.

## Threat model

The threat model is section 7 of the
[design document](docs/openboard-design-document.md#7-threat-model). It
describes the attackers Openboard is designed to resist and what is out of
scope. A report is most useful when it shows a break in a property that the
threat model claims.
