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

### Installing Codex Lab (this fork)

The official installers below install the stock `codex` command. To use the
multi-agent and lifecycle changes in this repository, build and install the
fork from source instead:

```shell
git clone https://github.com/mindorigin150/codex-lab.git
cd codex-lab
bash scripts/install/install-codex-lab.sh
```

The source build requires a working Rust toolchain with `cargo`. On Linux,
read-only explorer and reviewer agents also require Bubblewrap:

```shell
# Debian/Ubuntu
sudo apt-get update
sudo apt-get install -y bubblewrap
```

Make sure the launcher directory is on `PATH`, then validate and start the lab
build:

```shell
export PATH="$HOME/.local/bin:$PATH"
codex-lab --version
codex-lab doctor
codex-lab
```

The installer keeps the stock `codex` command untouched. It installs versioned
binaries under `~/.local/lib/codex-lab`, creates the
`~/.local/bin/codex-lab` launcher, and uses an isolated `~/.codex-lab`
configuration home. Recorded sessions and the SQLite conversation index are
shared with the default `~/.codex` home, so `codex-lab resume --all` can find
conversations created by either installation. Authentication and other lab
configuration may still need to be set up separately on first use.

To update an existing source installation, update the checkout and rerun the
same installer:

```shell
cd /path/to/codex-lab
git pull --rebase
bash scripts/install/install-codex-lab.sh
```

The installer builds a new versioned release and atomically switches the
`codex-lab` launcher to it. If upstream changes conflict with this fork, resolve
the source conflicts before rerunning the installer.

### Installing and running Codex CLI

Run the following on Mac or Linux to install Codex CLI:

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Run the following on Windows to install Codex CLI:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"
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
