# Luna

<img src="crates/zed/resources/luna/app-icon.svg" width="128" alt="Luna icon">

Luna is a lightweight code editor based on [Zed](https://github.com/zed-industries/zed).
It drops the AI and collaboration features, trims memory and CPU use, and adds features familiar from JetBrains IDEs.

## What's different

### Lighter

- **No AI features**: the agent, edit predictions, Copilot and friends aren't part of the default build (only with the `ai` build option).
- **No collaboration or zed.dev sign-in**: calls, channels and WebRTC are gone, and Luna doesn't connect to a server on startup (only with the `collab` build option).
- **Telemetry and auto-update off**: update checks are disabled entirely, so Zed's update server can't replace Luna with Zed.
- **Quiet when idle**: the cursor stops blinking without input, and the macOS display link pauses while nothing needs redrawing.
- **Leaner file watching**: bursts of file changes no longer pile up updates, symlink targets are scanned once, and Git internals are left alone.
- **Lighter language servers**
  - rust-analyzer: no up-front analysis of the whole workspace, check-on-save covers only the saved package, and idle servers above 6 GB are restarted.
  - TypeScript: no syntax-only second tsserver, no automatic type acquisition, and a lower memory cap.
  - ESLint: runs as a plugin inside tsserver instead of as its own server.
  - Tailwind: starts only in projects that depend on it.
  - Prettier: stops after 10 minutes without formatting.
- **Pausable indexing**: `workspace: toggle indexing` pauses file scanning and language servers, and background indexing waits on its own in Low Power Mode or under thermal pressure.

| Measured on macOS, Apple Silicon | Zed | Luna |
|---|---|---|
| Binary | 371 MB | 268 MB |
| CPU while idle | 0.5% | 0.07% |
| rust-analyzer peak memory (Zed's repository) | 10 GB | 4 GB |
| Expo project (editor + language servers) | 1.36 GB | 0.94 GB |

### JetBrains-like

- **Project symbol completions**: Luna's own symbol index completes names from the whole project, even before language servers are ready, and ranks what you picked before first. The index is cached, so a restart only re-reads changed files.
- **Show usages**: cmd-click on a definition lists its usages in a picker, and usage counts appear above functions.
- **TODO panel**: collects the project's TODO and FIXME comments.
- **Spell checking**: misspelled words in comments and strings are underlined, with fixes and Learn Spelling under `cmd-.` (uses the macOS spell checker).

### Also

- **Open in Ghostty**: opens the current project, or the selected folder, in a new Ghostty window.
- **Separate data**: Luna's settings and data don't mix with Zed's.
  - Settings: `~/.config/luna`
  - Data: `~/Library/Application Support/Luna`
  - Logs: `~/Library/Logs/Luna`

## Building

You need the Xcode command line tools, [rustup](https://rustup.rs) (the toolchain version comes from `rust-toolchain.toml`) and `cmake`.

```sh
script/bundle-luna-mac --install   # builds Luna.app and installs it to ~/Applications
```

For just the binary, `cargo build --release -p zed` produces `target/release/luna`.
The `ai` and `collab` build options bring back Zed's AI and collaboration features.

Zed's build docs still apply: [macOS](./docs/src/development/macos.md) · [Linux](./docs/src/development/linux.md) · [Windows](./docs/src/development/windows.md)

## License

Luna is a fork of [Zed](https://github.com/zed-industries/zed) by Zed Industries, Inc.
Like Zed, its source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.
Third-party dependency licenses are tracked with [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) and `script/licenses/zed-licenses.toml`.
