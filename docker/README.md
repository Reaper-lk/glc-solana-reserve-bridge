# `glc-bridge-daemon` deployment image

Build from the repo root (the build context needs both `shared/` and
`service/`):

```
docker build -f docker/Dockerfile -t glc-bridge-daemon .
```

Run with config and key files mounted read-only, and a named volume for
the SQLite ledger:

```
docker run --rm \
  -v /path/to/config.toml:/etc/glc-bridge/config.toml:ro \
  -v /path/to/keys:/etc/glc-bridge/keys:ro \
  -v glc-bridge-data:/var/lib/glc-bridge \
  -p 127.0.0.1:9100:9100 \
  glc-bridge-daemon --config /etc/glc-bridge/config.toml
```

Point `config.toml`'s `service.db_path` and `operators.*_key_paths` at
the mounted paths above (e.g. `/var/lib/glc-bridge/ledger.sqlite3` and
`/etc/glc-bridge/keys/...`).

## Key posture

This image never bakes in config or key material — both are mounted at
runtime, read-only. Key loading itself is still DEV/TEST posture
(`service/src/config.rs` module docs, docs/16-p0-checkpoint.md P2): do
not point it at production custody keys until the HSM/KMS work lands.

## Build verification status

This Dockerfile was written and its build command
(`cargo build --release --bin glc-bridge-daemon` from `service/`) is the
same one verified working natively throughout Phase 6/P0/P1 development.
The containerization layer itself (base images, `apt-get`, non-root user)
has **not** been verified with an actual `docker build` in this
repository's automated sessions so far — the sandbox those sessions ran
in could not reach the Docker daemon socket. Run a real `docker build`
before relying on this image for a rehearsal or deployment.
