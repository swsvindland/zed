# Patched tiberius

This is tiberius 0.13.0 as published on crates.io (upstream commit
`7fd75bb7070fe6ea41374698abf78928bb983bda` in https://github.com/prisma/tiberius), used through
`[patch.crates-io]` in the workspace `Cargo.toml`. Only `src/`, the manifest, licenses, README
and changelog are kept; the manifest's `[[example]]` and `[[test]]` targets were removed because
those files aren't vendored. It isn't a workspace member, so workspace lints and clippy don't
apply to it.

## Changes

- **`cancel_query` reads the Attention acknowledgement wherever it arrives**
  (`src/client/connection.rs`, `src/tds/stream/token.rs`). When a request is still running,
  SQL Server ends its response with a `DONE` (status `0x0002`) in one TDS message and sends the
  acknowledging `DONE` (status `0x0020`, `DONE_ATTN`) as a second message. Upstream stops at the
  end of the first message and fails with "Never got a DONE token acknowledging the Attention
  signal", leaving the acknowledgement to be read as the next request's reply.
  `Connection::cancel_request` now reads on into the next message until the acknowledgement
  arrives. An `ERROR` token from the cancelled request no longer fails the cancel either: its
  results are discarded like the rest of the response. Unit tests are in `token.rs`
  (`cancel_request_*`).

## Updating

To move to a newer tiberius, check whether upstream has fixed `cancel_request`. If not, copy the
new release's files here and reapply the change above. The database crate's LocalDB tests
(`crates/database/tests/local_db.rs`, opt in with `ZED_DATABASE_TEST_SQL_SERVER`) cover it end
to end.
