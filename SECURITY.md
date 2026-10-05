# Security policy

## Reporting a vulnerability

Write to **[security@tessaridb.com](mailto:security@tessaridb.com)**. Please do not open a public issue for a
vulnerability.

Say which release you ran (`tessaridb --version`), how the node was started, and what you observed. A statement
or a request that reproduces it is the most useful thing you can send. We reply, tell you what we found, and agree
a disclosure date with you before anything is published; a fix ships as a release whose CHANGELOG names the issue
under **Security**.

## Supported versions

TessariDB is a beta. Fixes go into the next release and are not backported: the supported version is the latest
tag on `main`.

## What a deployment owes its own security

The engine's defaults assume an operator reads them. A few that matter most:

- **A store with no user is open**, for reads as well as writes. Declare a store-wide owner first.
- **Give every exposed node a certificate** (`--tls-cert` / `--tls-key`). Without one it serves in the clear and
  says so when it starts.
- **Private key files must be readable by their owner alone** (`chmod 600`); a node refuses one that is not.
- **The peer link is always mutual TLS**; a node joins only when the cluster has declared it.

The documentation at [docs.tessaridb.com](https://docs.tessaridb.com) covers each of these in full.
