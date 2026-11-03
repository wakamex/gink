# signal-store

The storage behind `signal-client`: one encrypted SQLite database per Signal account, holding the account's registration, sessions, keys, contacts and groups.

It began as a copy of the `presage-store-sqlite` crate from [presage](https://github.com/whisperfish/presage) at commit `b44d0129348fffde7455214bc081859dcf2fd242` (2026-10-10), copyright its contributors and licensed AGPL-3.0-only, and is changed here as gsignal needs. The licence text is `LICENSE` at the repository root.

`.sqlx/` holds the checked query metadata that lets the `sqlx` macros compile without a database. After changing a query or a migration, regenerate it with `cargo sqlx prepare`.
