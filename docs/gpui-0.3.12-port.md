# GPUI 0.3.12 migration worktree

This worktree builds on PR #15 at `50a6371a9a9ad2d79ca05860d2e658ca5a19c681`.
PR #15's tested default stays `gpui-pre` 0.3.9 / GPUI Kit 0.8.0 until
new vendor patches and cross-platform checks pass. Do **not** change the
supported-version claim to 0.3.12 or enable `modern-input` against 0.3.9.

## Completed integration preparation

- `gpui-mcp` has a `modern-input` feature for the input API introduced in
  `gpui-pre` 0.3.10. Runtime keystrokes are parsed with
  `Keystroke::parse_with_os` and the running `App`'s OS. Syntax validation
  uses Linux solely to validate syntax; dispatch reparses using the real OS.
- Modern synthetic wheel input uses `PlatformInput::Scroll(ScrollEvent)` with
  `ScrollSource::Pointer`. The legacy feature path continues using
  `PlatformInput::ScrollWheel(ScrollWheelEvent)`. The HTML runtime regression
  harness uses the same feature switch.
- An owned-string keystroke regression test prevents accidentally restoring
  the new GPUI `Keystroke::parse(&'static str)` lifetime restriction.
- `scripts/port-gpui-pre.py` can produce candidate **per-version** patch sets
  with three-way Git cherry-picks. It reconstructs the original source from
  the vendored 0.3.9 snapshot, preserves the independence of `automation`,
  `font-fallback`, and `grid`, and does not silently resolve conflicts.

## Still required to declare 0.3.12 support

1. Obtain the published `gpui-pre` archives for 0.3.10, 0.3.11, and 0.3.12.
   These are distinct versions and must be verified by their real SHA-256
   checksums. The 0.3.12 release corresponds to Zed
   `d2a4fb6ded7c470be25d1541281da3882495b65b`.
2. Run the port tool once per release (online, or using a locally downloaded
   `.crate` file), using a fresh directory each time:

   ```sh
   python3 scripts/port-gpui-pre.py 0.3.12 \
     --archive /path/to/gpui-pre-0.3.12.crate \
     --work /tmp/gpui-pre-0.3.12-port
   ```

   The tool writes candidate patches into `--work/patches`. If a cherry-pick
   conflicts, inspect the resulting worktree, manually port the relevant
   observer/behavior/ARIA changes, continue the cherry-pick, and rerun with
   `--finalize`. **Do not** accept the tool's output without reviewing it.
3. Add the verified version entries to `vendor/gpui-pre.json`, copy tested
   patch sets into `vendor/patches/gpui-pre/<version>/`, adjust the workspace
   dependency requirements, and regenerate `vendor/gpui-pre` with
   `cargo xtask vendor`. Then run `cargo xtask vendor --check`. The upstream
   archive's `zed_rev` and checksum should match the metadata.
4. Update the separate GPUI Kit example from 0.8.0 to 0.9.1, update the
   `gpui-base` release range, and regenerate both workspace lockfiles.
   Select `gpui-mcp/modern-input` (and `gpui-mcp-html/modern-input` for the
   runtime tests) on the new release; **never** mix feature sets in one Cargo
   build. Update `xtask/src/consumer.rs` with the new feature selection.
5. Port all remaining frame-observer, accessibility, element, and window
   additions from the three vendor patches. Upstream API changes can affect
   the render pipeline and text system independently of input. The patch
   generator cannot establish correctness of these changes.
6. Run the entire CI matrix on Windows, macOS, Linux X11 and Linux Wayland,
   including the patch-series optional-patch permutations, test-support,
   integration tests, consumer checks, and visual parity tests. Confirm
   compatibility with Rust 1.95 and 1.96 or explicitly raise the MSRV.
7. Refresh the direct `zed` backend only as a **separate** migration, pinning
   `gpui` and `gpui_platform` to the same newer Zed revision. The `gpui-ce`
   backend should remain on its own compatibility track.

### Minimum regression commands

```sh
cargo xtask vendor --crate gpui-pre --check
cargo check -p gpui-mcp --no-default-features --features gpui-pre,modern-input --locked
cargo test -p gpui-mcp --no-default-features --features gpui-pre,modern-input,test-support --locked
cargo test -p gpui-mcp-html -p gpui-view-transitions --no-default-features \
  --features gpui-mcp-html/gpui-pre,gpui-mcp-html/modern-input,gpui-mcp-html/json,gpui-mcp-html/ron --locked
cargo test --manifest-path examples/gpui-kit/Cargo.toml --locked
cargo fmt --all -- --check
cargo clippy -p gpui-mcp --all-targets --no-default-features \
  --features gpui-pre,modern-input,test-support --locked -- -D warnings
```

### Unverified environment constraints

This worktree was produced without a Rust toolchain or access to crates.io.
Python syntax checks, Git diff whitespace checks, and an **offline synthetic
0.3.9 patch-port round-trip** passed. The published GPUI 0.3.12 archive was
not downloaded, no real 0.3.12 patch was generated, and no Rust build/test
or Wayland runtime test was executed. These omissions are release blockers.
