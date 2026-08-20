<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you want the desktop app experience, run <code>codex app</code> or visit <a href="https://chatgpt.com/codex?app-landing-page=true">the Codex App page</a>.
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Quickstart

### Installing Codex Lab

Install the latest precompiled Codex Lab release on Linux, macOS, or Windows
through a POSIX-compatible shell:

```shell
curl -fsSL https://raw.githubusercontent.com/mindorigin150/codex-lab/main/scripts/install/install-codex-lab.sh | sh
```

The installer selects the matching Linux musl, macOS, or Windows release
target, verifies the package SHA-256 checksum, and installs a versioned package
under `~/.local/lib/codex-lab` with an atomic `codex-lab` launcher update. It
uses `~/.codex` for shared state by default and leaves the stock `codex`
command unchanged. Set `CODEX_LAB_RELEASE=VERSION` to install a specific
`codex-lab-vVERSION` release. Each Lab release is built once per target
architecture; Linux distributions do not require separate builds.

To build the current checkout instead, clone the repository and opt into the
source path explicitly:

```shell
git clone https://github.com/mindorigin150/codex-lab.git
cd codex-lab
bash scripts/install/install-codex-lab.sh --source
```

Use `--binary PATH` to install a locally built matching binary without running
Cargo. On Linux, source installs can bundle a trusted Bubblewrap binary with
`--bwrap PATH`.

Codex Lab also carries the Lab delegation behavior on top of the current
upstream Codex: built-in `explorer` and `reviewer` agents are read-only,
explorers always start with fresh context, and blocking analysis work is not
silently abandoned when the parent turn would otherwise finish.

When the selected model reports that it is at capacity, Lab classifies the HTTP,
SSE, or WebSocket response as a recoverable overload and retries indefinitely
with bounded backoff until the request succeeds or the user interrupts it. The
UI keeps the turn active and shows `Reconnecting... overload attempt N` as retry
detail; it does not convert this recoverable condition into a completed task or
a final error.

To publish a precompiled Lab release, push a tag such as
`codex-lab-v0.1.0`. The release workflow builds each supported target once and
publishes the package archives and checksum manifest consumed by the one-line
installer.

### Installing and running Codex CLI

Run the following on Mac or Linux to install Codex CLI:

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Run the following on Windows to install Codex CLI:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"
```

The standalone installers download from `https://releases.openai.com/codex` by default and fall back to GitHub Releases if a metadata or asset download is unavailable. To force GitHub Releases, set `CODEX_INSTALLER_USE_RELEASES_OPENAI_COM` to `false` (`0` and `no` are also accepted):

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_INSTALLER_USE_RELEASES_OPENAI_COM=false sh
```

```powershell
$env:CODEX_INSTALLER_USE_RELEASES_OPENAI_COM='false'; irm https://chatgpt.com/codex/install.ps1 | iex
```

Codex CLI can also be installed via the following package managers:

```shell
# Install using npm
npm install -g @openai/codex
```

```shell
# Install using Homebrew
brew install --cask codex
```

Then simply run `codex` to get started.

<details>
<summary>You can also go to the <a href="https://github.com/openai/codex/releases/latest">latest GitHub Release</a> and download the appropriate binary for your platform.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `codex-x86_64-unknown-linux-musl`), so you likely want to rename it to `codex` after extracting it.

</details>

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
