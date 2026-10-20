# TablePort

A fast, native PostgreSQL client for macOS. Free and open source.

> **Public beta.** TablePort is new. Try it on development data first, and
> please [report problems](https://github.com/morshedx/tableport/issues/new?template=bug_report.yml).

## Features

- **Connections**: paste a `postgres://` URL, TLS (disable, prefer, require,
  verify-full), SSH tunnels with a password or key, colour tags, read-only
  mode. Passwords stay in the macOS Keychain.
- **Data grid**: sort, filter, page through large tables, select cells, edit
  in place or in a cell editor for long text and JSON, follow foreign keys.
- **Safe changes**: edits, new rows and deletes stay pending until you review
  the SQL and commit them in one transaction. Production connections ask first.
- **Query editor**: run the statement under the cursor or everything,
  cancel, beautify, line numbers, errors marked in place, history and saved
  queries.
- **Copy and export**: copy rows as CSV or `INSERT` statements; export a
  table to CSV, JSON or SQL.
- **Fast**: switching between large tables takes about 60–100 ms.

## Install

1. Download the latest `.dmg` from
   [Releases](https://github.com/morshedx/tableport/releases/latest).
2. Open it and drag **TablePort** to **Applications**.
3. The first time, macOS blocks the app because it isn't notarized yet. Open
   **System Settings › Privacy & Security** and click **Open Anyway**.

TablePort updates itself (Settings › Updates). It needs macOS 14 or later and
an Apple silicon Mac.

## Build

TablePort is a Swift app (AppKit and SwiftUI) on a Rust core that does all
the database work, connected with [UniFFI](https://mozilla.github.io/uniffi-rs/).

Requirements: Xcode command-line tools, Rust (`rustup`), and mac-kit (the
author's shared build, settings and update kit) checked out next to this
repository. mac-kit is not public yet, so building from source is not possible
outside the author's machine for now.

```sh
make run          # build the Rust core, the app, and open it
make core-test    # Rust tests; needs a local PostgreSQL that trusts your user
```

To sign builds with your own certificate, create `local.mk`:

```make
MACKIT_SIGN = Apple Development: you@example.com (TEAMID1234)
```

## Privacy

Connections, query history and saved queries stay on your Mac. Passwords stay
in the macOS Keychain. TablePort goes online only to reach your own database
servers and to check for updates.

## Licence

[Apache-2.0](LICENSE). "TablePort" and its icon are trademarks of the author.
