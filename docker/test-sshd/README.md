# SSH integration fixture

Run the Rust suites from the repository root, sequentially with other Cargo gates:

```text
cargo test -p adit-ssh --lib
cargo test -p adit-session jump_auth::tests
cargo test -p adit-ui jump_auth_ui::tests
cargo test -p adit-ssh --features integration --test integration -- --test-threads=4
```

The integration suite requires a Docker daemon running Linux containers. The
agent case additionally requires Unix and the OpenSSH `ssh-agent`/`ssh-add`
executables. It uses a new socket and an explicitly selected agent, without
changing process-global environment or loading keys into the user's agent.

Each image has a unique `adit-test-sshd:run-...` tag. Containers and networks have
unique names and the `io.adit.fixture.owner` label. RAII guards remove only
recorded resources whose owner label still matches. Cleanup never uses prune,
image-name prefixes, or the historical `adit-test-sshd:latest` tag. Anonymous
volumes, if any, are removed only together with their owned container.

The default sshd supports password/public-key auth, SFTP and forwarding.
Tests can enable a PAM stack containing `pam_unix` and `pam_adit_fixture.so` in
their own container. The module asks a deterministic verification-code question;
the expected public fixture answer lives inside that container. This is a
challenge/response protocol fixture, not TOTP, and does not assert whether a real
TOTP server would accept a repeated code. Each new SSH connection must ask again.

The Docker tests cover standalone SFTP/tunnel MFA, cancellation/disconnect,
rejection, independently owned two-hop credentials, SOCKS5 IPv4/remote DNS, and
agent-to-OpenSSH authentication. Synthetic unit tests separately cover reply
ownership, stale answers, missing-owner fallback, partial authentication and
secret serialization/debug boundaries.
