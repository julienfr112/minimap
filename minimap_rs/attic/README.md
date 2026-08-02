# attic

Programs that were needed once. Kept because they record *why* something in the
pipeline looks the way it does, not because they still build — they are outside
`examples/` precisely so cargo does not try.

| file | what replaced it |
| --- | --- |
| `fetch-europe.sh` | `minimap download --europe`. The script existed because the Rust step could not resume, retry or run transfers concurrently; `download.rs` does all three now, so there is one way to fetch an extract instead of two that disagreed about where it lands. |
| `add-land.rs` | `load.rs`. A migration that spliced a `land` layer into an archive that had already cost eight hours to bake. `land` is a layer like any other now. |
| `add-places.rs` | `load.rs`, same story for the label layer. |
| `q.rs`, `qw.rs` | `minimap sql "..."`. Two near-identical programs whose only job was to open the database and print a query — and which kept drifting from the real connection settings, which is the one bug a probe must not have. |
| `dbsize.rs` | `minimap info`. |

The migrations are the interesting ones. Both exist because the old layout had
no way to add a layer without rebuilding everything, and no cheap way to tell
what state a build was in. `make` answers the first (stages are stamped, so only
what changed re-runs) and `minimap info` the second.
