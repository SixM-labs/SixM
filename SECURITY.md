# Security policy

SixM runs code from server owners and talks to untrusted clients over the
internet, so security reports are taken seriously.

## Reporting a vulnerability

Please do **not** open a public issue for security problems.

Report them privately through GitHub: open the repository's **Security** tab
and choose **Report a vulnerability**. Include:

- what an attacker can do (for example crash the server, read files, run code,
  impersonate a player)
- the SixM version or commit
- steps or a minimal resource / client message that reproduces it

You will get an answer within seven days. Fixes for confirmed issues are
released as soon as possible, and reporters are credited unless they ask not
to be.

## Supported versions

SixM is in early development. Only the latest commit on `main` receives
security fixes.

## Scope

In scope:

- the server (`sixm-server`), the network layer (`sixm-net`), the resource
  loader (`sixm-resources`) and the Lua runtime (`sixm-lua`)
- escapes from the Lua sandbox, for example reaching `io`, `os.execute`,
  loading bytecode or accessing files outside a resource
- anything a client can do over the network beyond what scripts allow it:
  forged function references, writes to state bags, crashes, memory or CPU
  exhaustion that the rate and size limits should prevent

Out of scope:

- server owners running malicious resources on their own server (resources
  are trusted code within their sandbox)
- unpinned client connections being open to man-in-the-middle attacks; this
  is documented, and certificate pinning is the fix
- denial of service that needs more bandwidth than the attacker's own link
