#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT="${CODEX_LAB_REPO_ROOT:-$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)}"
CODEX_RS_DIR="$REPO_ROOT/codex-rs"

GITHUB_REPOSITORY="mindorigin150/codex-lab"
GITHUB_API_BASE="https://api.github.com/repos/$GITHUB_REPOSITORY/releases"
GITHUB_DOWNLOAD_BASE="https://github.com/$GITHUB_REPOSITORY/releases/download"
RELEASE_VERSION="${CODEX_LAB_RELEASE:-latest}"
RELEASE_TAG=""
RELEASE_TAG_PREFIX="${CODEX_LAB_RELEASE_TAG_PREFIX:-codex-lab-v}"
RELEASE_MODE=true
TARGET="${CODEX_LAB_TARGET:-}"

SHARED_STATE_HOME="${CODEX_SHARED_STATE_HOME:-$HOME/.codex}"
LAB_HOME="${CODEX_LAB_HOME:-$SHARED_STATE_HOME}"
INSTALL_ROOT="${CODEX_LAB_INSTALL_ROOT:-$HOME/.local/lib/codex-lab}"
BIN_DIR="${CODEX_LAB_BIN_DIR:-$HOME/.local/bin}"
SOURCE_BINARY="${CODEX_LAB_BINARY:-}"
SOURCE_BWRAP="${CODEX_LAB_BWRAP:-}"
RELEASE_ID="${CODEX_LAB_RELEASE_ID:-}"
RUN_DOCTOR="${CODEX_LAB_RUN_DOCTOR:-true}"
STRIP_BINARY="${CODEX_LAB_STRIP_BINARY:-true}"

step() {
  printf '==> %s\n' "$1"
}

warn() {
  printf 'WARNING: %s\n' "$1" >&2
}

usage() {
  cat <<'EOF'
Usage: install-codex-lab.sh [OPTIONS]

Install the prebuilt Codex Lab release for this host as `codex-lab`.

By default Codex Lab uses the official ~/.codex home, so configuration,
authentication, conversation history, rollouts, and SQLite-backed state are
shared with `codex`. Set CODEX_LAB_HOME explicitly to keep a separate config
home while retaining the shared state home.

Options:
  --release VERSION   Install a release version (default: latest).
  --source            Build the current checkout with Cargo.
  --binary PATH       Install a matching local Codex Lab binary.
  --target TARGET     Release target override (for example x86_64-unknown-linux-musl).
  --bwrap PATH        Bundle this bubblewrap binary on Linux.
  --release-id ID     Versioned install directory name (default: git commit).
  --skip-doctor       Do not run the installed binary's doctor command.
  --no-strip          Do not strip debug symbols from the installed copy.
  -h, --help          Show this help.

Environment:
  CODEX_LAB_HOME          Optional isolated lab config home (default: ~/.codex).
  CODEX_SHARED_STATE_HOME Shared official Codex home (default: ~/.codex).
  CODEX_LAB_INSTALL_ROOT  Versioned install root.
  CODEX_LAB_BIN_DIR       Directory for the codex-lab launcher.
  CODEX_LAB_BINARY        Same as --binary.
  CODEX_LAB_RELEASE        Release version; defaults to latest.
  CODEX_LAB_TARGET         Release target override.
  CODEX_LAB_REPO_ROOT      Checkout root used by --source.
  CODEX_LAB_BWRAP         Same as --bwrap.
  CODEX_LAB_RELEASE_ID    Same as --release-id.
  CODEX_LAB_RUN_DOCTOR    true/false; default true.
  CODEX_LAB_STRIP_BINARY  true/false; default true.
EOF
}

is_true() {
  case "$1" in
    1 | true | TRUE | yes | YES) return 0 ;;
    *) return 1 ;;
  esac
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --binary)
        [ "$#" -ge 2 ] || {
          echo "--binary requires a path." >&2
          exit 1
        }
        SOURCE_BINARY="$2"
        RELEASE_MODE=false
        shift
        ;;
      --source)
        RELEASE_MODE=false
        ;;
      --release)
        [ "$#" -ge 2 ] || {
          echo "--release requires a value." >&2
          exit 1
        }
        RELEASE_VERSION="$2"
        RELEASE_MODE=true
        shift
        ;;
      --target)
        [ "$#" -ge 2 ] || {
          echo "--target requires a value." >&2
          exit 1
        }
        TARGET="$2"
        shift
        ;;
      --bwrap)
        [ "$#" -ge 2 ] || {
          echo "--bwrap requires a path." >&2
          exit 1
        }
        SOURCE_BWRAP="$2"
        shift
        ;;
      --release-id)
        [ "$#" -ge 2 ] || {
          echo "--release-id requires a value." >&2
          exit 1
        }
        RELEASE_ID="$2"
        shift
        ;;
      --skip-doctor)
        RUN_DOCTOR=false
        ;;
      --no-strip)
        STRIP_BINARY=false
        ;;
      --help | -h)
        usage
        exit 0
        ;;
      *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
    esac
    shift
  done
}

validate_release_id() {
  case "$1" in
    '' | *[!A-Za-z0-9._-]*)
      echo "Invalid release id: $1" >&2
      exit 1
      ;;
  esac
}

default_release_id() {
  if command -v git >/dev/null 2>&1 && git -C "$REPO_ROOT" rev-parse HEAD >/dev/null 2>&1; then
    commit=$(git -C "$REPO_ROOT" rev-parse HEAD)
    if git -C "$REPO_ROOT" diff --quiet --ignore-submodules -- &&
      git -C "$REPO_ROOT" diff --cached --quiet --ignore-submodules --; then
      printf '%s\n' "$commit"
    else
      printf '%s-dirty-%s\n' "$commit" "$(date +%Y%m%d%H%M%S)"
    fi
  else
    printf 'local-%s\n' "$(date +%Y%m%d%H%M%S)"
  fi
}

sha256_file() {
  path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
  else
    printf 'unavailable\n'
  fi
}

download_text() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --connect-timeout 10 --max-time 30 "$url"
    return
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -q -O - "$url"
    return
  fi
  echo "curl or wget is required to install a Codex Lab release." >&2
  exit 1
}

download_file() {
  url="$1"
  output="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --connect-timeout 10 --max-time 300 "$url" -o "$output"
    return
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -q -O "$output" "$url"
    return
  fi
  echo "curl or wget is required to install a Codex Lab release." >&2
  exit 1
}

normalize_release_version() {
  case "$1" in
    codex-lab-v*)
      printf '%s\n' "${1#codex-lab-v}"
      ;;
    "" | latest)
      printf 'latest\n'
      ;;
    rust-v*)
      printf '%s\n' "${1#rust-v}"
      ;;
    v*)
      printf '%s\n' "${1#v}"
      ;;
    *)
      printf '%s\n' "$1"
      ;;
  esac
}

validate_release_version() {
  version="$1"
  if [ "$version" = latest ]; then
    return
  fi
  if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-alpha(\.[0-9]+){0,2}|-beta(\.[0-9]+)?)?$'; then
    echo "Invalid Codex Lab release version: $version" >&2
    exit 1
  fi
}

json_tag_name() {
  awk '
    {
      for (i = 1; i <= length($0); i++) {
        char = substr($0, i, 1)
        if (in_string) {
          if (escaped) {
            token = token char
            escaped = 0
          } else if (char == "\\") {
            escaped = 1
          } else if (char == "\"") {
            in_string = 0
            if (string_is_value) {
              if (object_depth == 1 && key == "tag_name") {
                print token
                exit
              }
            } else {
              pending_key = token
            }
          } else {
            token = token char
          }
          continue
        }

        if (char == "\"") {
          in_string = 1
          token = ""
          escaped = 0
          string_is_value = expecting_value
        } else if (char == "{") {
          object_depth++
          expecting_value = 0
        } else if (char == "}") {
          object_depth--
          expecting_value = 0
          key = ""
          pending_key = ""
        } else if (char == ":" && pending_key != "") {
          key = pending_key
          pending_key = ""
          expecting_value = 1
        } else if (char == ",") {
          expecting_value = 0
          key = ""
          pending_key = ""
        }
      }
    }
  '
}

json_asset_digest() {
  asset="$1"
  awk -v wanted="$asset" '
    {
      for (i = 1; i <= length($0); i++) {
        char = substr($0, i, 1)
        if (in_string) {
          if (escaped) {
            token = token char
            escaped = 0
          } else if (char == "\\") {
            escaped = 1
          } else if (char == "\"") {
            in_string = 0
            if (string_is_value) {
              if (asset_object_depth == object_depth) {
                if (key == "name") asset_name = token
                if (key == "digest") asset_digest = token
              }
            } else {
              pending_key = token
            }
          } else {
            token = token char
          }
          continue
        }

        if (char == "\"") {
          in_string = 1
          token = ""
          escaped = 0
          string_is_value = expecting_value
        } else if (char == "{") {
          object_depth++
          if (assets_array_depth != 0 && array_depth == assets_array_depth && asset_object_depth == 0) {
            asset_object_depth = object_depth
            asset_name = ""
            asset_digest = ""
          }
          expecting_value = 0
        } else if (char == "}") {
          if (object_depth == asset_object_depth && asset_name == wanted) {
            print asset_digest
            exit
          }
          if (object_depth == asset_object_depth) asset_object_depth = 0
          object_depth--
          expecting_value = 0
          key = ""
          pending_key = ""
        } else if (char == "[") {
          array_depth++
          if (key == "assets" && object_depth == 1) assets_array_depth = array_depth
          expecting_value = 0
        } else if (char == "]") {
          if (array_depth == assets_array_depth) assets_array_depth = 0
          array_depth--
          expecting_value = 0
          key = ""
          pending_key = ""
        } else if (char == ":" && pending_key != "") {
          key = pending_key
          pending_key = ""
          expecting_value = 1
        } else if (char == ",") {
          expecting_value = 0
          key = ""
          pending_key = ""
        }
      }
    }
  '
}

resolve_release() {
  normalized_version=$(normalize_release_version "$RELEASE_VERSION")
  validate_release_version "$normalized_version"

  if [ "$normalized_version" = latest ]; then
    metadata_url="$GITHUB_API_BASE/latest"
    requested_release=latest
  else
    RELEASE_TAG="${RELEASE_TAG_PREFIX}${normalized_version}"
    metadata_url="$GITHUB_API_BASE/tags/$RELEASE_TAG"
    requested_release="$normalized_version"
  fi

  if ! release_metadata=$(download_text "$metadata_url"); then
    echo "Could not fetch Codex Lab release metadata for $requested_release." >&2
    exit 1
  fi

  if [ "$normalized_version" = latest ]; then
    RELEASE_TAG=$(printf '%s\n' "$release_metadata" | json_tag_name)
    normalized_version=$(normalize_release_version "$RELEASE_TAG")
    if [ "$normalized_version" = latest ] || [ "$normalized_version" = "$RELEASE_TAG" ]; then
      echo "Could not resolve the latest Codex Lab release tag." >&2
      exit 1
    fi
    validate_release_version "$normalized_version"
  fi

  if [ -z "$RELEASE_TAG" ]; then
    RELEASE_TAG="${RELEASE_TAG_PREFIX}${normalized_version}"
  fi
  RELEASE_VERSION="$normalized_version"
}

resolve_host_target() {
  [ -n "$TARGET" ] && return

  system=$(uname -s)
  machine=$(uname -m)
  case "$system" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    MINGW* | MSYS* | CYGWIN*) os=windows ;;
    *)
      echo "Unsupported host operating system: $system" >&2
      exit 1
      ;;
  esac

  case "$machine" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *)
      echo "Unsupported host architecture: $machine" >&2
      exit 1
      ;;
  esac

  case "$os:$arch" in
    linux:x86_64) TARGET=x86_64-unknown-linux-musl ;;
    linux:aarch64) TARGET=aarch64-unknown-linux-musl ;;
    darwin:x86_64) TARGET=x86_64-apple-darwin ;;
    darwin:aarch64) TARGET=aarch64-apple-darwin ;;
    windows:x86_64) TARGET=x86_64-pc-windows-msvc ;;
    windows:aarch64) TARGET=aarch64-pc-windows-msvc ;;
    *)
      echo "No Codex Lab release target for $system/$machine." >&2
      exit 1
      ;;
  esac
}

manifest_digest() {
  manifest="$1"
  asset="$2"
  awk -v asset="$asset" '
    $1 ~ /^[0-9a-fA-F]{64}$/ && ($2 == asset || $2 == "*" asset) {
      print tolower($1)
      found = 1
      exit
    }
    END { if (!found) exit 1 }
  ' "$manifest"
}

verify_file_digest() {
  path="$1"
  expected="$2"
  actual=$(sha256_file "$path")
  if [ "$actual" = unavailable ]; then
    echo "sha256sum or shasum is required to verify release assets." >&2
    exit 1
  fi
  if [ "$actual" != "$expected" ]; then
    echo "SHA-256 mismatch for $(basename "$path"): expected $expected, got $actual." >&2
    exit 1
  fi
}

bwrap_is_compatible() {
  candidate="$1"
  [ -f "$candidate" ] && [ -x "$candidate" ] || return 1
  "$candidate" --help 2>&1 | grep -q -- '--perms'
}

resolve_bwrap_source() {
  if [ "$(uname -s 2>/dev/null || true)" != Linux ]; then
    if [ -n "$SOURCE_BWRAP" ]; then
      echo "--bwrap is only supported on Linux." >&2
      exit 1
    fi
    return
  fi

  if [ -n "$SOURCE_BWRAP" ]; then
    if ! bwrap_is_compatible "$SOURCE_BWRAP"; then
      echo "Configured bwrap is not executable or does not support --perms: $SOURCE_BWRAP" >&2
      exit 1
    fi
    BWRAP_SOURCE_KIND=explicit
    return
  fi

  official_bwrap="$SHARED_STATE_HOME/packages/standalone/current/codex-resources/bwrap"
  if bwrap_is_compatible "$official_bwrap"; then
    SOURCE_BWRAP="$official_bwrap"
    BWRAP_SOURCE_KIND=official-codex
    return
  fi

  path_bwrap=$(command -v bwrap 2>/dev/null || true)
  if [ -n "$path_bwrap" ] && bwrap_is_compatible "$path_bwrap"; then
    SOURCE_BWRAP="$path_bwrap"
    BWRAP_SOURCE_KIND=path
    return
  fi

  cat >&2 <<EOF
No compatible bubblewrap binary was found for the Linux read-only sandbox.
Install the official Codex standalone package first, or rerun this installer
with --bwrap PATH pointing to a trusted user-owned bwrap binary. No sudo or
system-wide package installation is required.
EOF
  exit 1
}

verify_codex_bwrap_digest() {
  binary="$1"
  embedded_bwrap_sha256=$(
    "$binary" debug bwrap-digest 2>/dev/null
  ) || {
    echo "Codex binary cannot report its embedded bwrap digest: $binary" >&2
    exit 1
  }
  if [ "$embedded_bwrap_sha256" != "$BWRAP_SHA256" ]; then
    cat >&2 <<EOF
Codex binary and bundled bwrap do not match.
  binary:         $binary
  binary expects: $embedded_bwrap_sha256
  bwrap digest:   $BWRAP_SHA256
Build Codex Lab with this installer, or provide the bwrap packaged with that binary.
EOF
    exit 1
  fi
}

replace_path_with_symlink() {
  link_path="$1"
  link_target="$2"
  tmp_link="$3"

  rm -f "$tmp_link"
  ln -s "$link_target" "$tmp_link"
  if mv -Tf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi
  if mv -hf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi
  rm -f "$link_path"
  mv -f "$tmp_link" "$link_path"
}

check_shared_rollout_dir() {
  name="$1"
  shared_dir="$SHARED_STATE_HOME/$name"
  lab_path="$LAB_HOME/$name"

  if [ -L "$lab_path" ]; then
    current_target=$(readlink "$lab_path" 2>/dev/null || true)
    if [ "$current_target" != "$shared_dir" ]; then
      echo "$lab_path already links to $current_target; expected $shared_dir." >&2
      exit 1
    fi
    return
  fi

  if [ -e "$lab_path" ] &&
    { [ ! -d "$lab_path" ] || [ -n "$(find "$lab_path" -mindepth 1 -maxdepth 1 -print -quit)" ]; }; then
    cat >&2 <<EOF
$lab_path already contains data and will not be replaced.
Merge or back up that directory manually, then rerun the installer.
EOF
    exit 1
  fi
}

ensure_shared_rollout_dir() {
  name="$1"
  shared_dir="$SHARED_STATE_HOME/$name"
  lab_path="$LAB_HOME/$name"

  if [ "$lab_path" = "$shared_dir" ]; then
    mkdir -p "$shared_dir"
    return
  fi

  check_shared_rollout_dir "$name"
  mkdir -p "$shared_dir"
  if [ -L "$lab_path" ]; then
    return
  fi

  if [ -e "$lab_path" ]; then
    rmdir "$lab_path"
  fi

  ln -s "$shared_dir" "$lab_path"
}

ensure_multi_agent_namespace() {
  config="$LAB_HOME/config.toml"

  if [ -L "$config" ] && [ ! -e "$config" ]; then
    echo "$config is a dangling symlink; refusing to write through it." >&2
    exit 1
  fi

  if [ -e "$config" ]; then
    if [ ! -f "$config" ]; then
      echo "$config exists but is not a regular file." >&2
      exit 1
    fi
    if grep -Eq '(^|[^A-Za-z0-9_])multi_agent_v2([^A-Za-z0-9_]|$)' "$config"; then
      return
    fi

    if ! (umask 077 && set -C && : >"$CONFIG_TMP") 2>/dev/null; then
      echo "Could not create temporary config: $CONFIG_TMP" >&2
      exit 1
    fi
    cat "$config" >"$CONFIG_TMP"
    cat >>"$CONFIG_TMP" <<'EOF'

[features.multi_agent_v2]
enabled = true
tool_namespace = "agents"
EOF
    chmod 0600 "$CONFIG_TMP"
    mv -f "$CONFIG_TMP" "$config"
    return
  fi

  if ! (umask 077 && set -C && : >"$CONFIG_TMP") 2>/dev/null; then
    echo "Could not create temporary config: $CONFIG_TMP" >&2
    exit 1
  fi
  cat >"$CONFIG_TMP" <<'EOF'
[features.multi_agent_v2]
enabled = true
tool_namespace = "agents"
EOF
  chmod 0600 "$CONFIG_TMP"

  if ln "$CONFIG_TMP" "$config" 2>/dev/null; then
    rm -f "$CONFIG_TMP"
    return
  fi

  rm -f "$CONFIG_TMP"
  if [ -e "$config" ] && [ -f "$config" ]; then
    return
  fi
  echo "Could not publish config without overwriting another path: $config" >&2
  exit 1
}

install_launcher() {
  launcher="$BIN_DIR/codex-lab"
  staged_launcher="$BIN_DIR/.codex-lab.$$"
  backup_stamp=$(date +%Y%m%d-%H%M%S)

  mkdir -p "$BIN_DIR"
  cat >"$staged_launcher" <<EOF
#!/bin/sh
set -eu

DEFAULT_CODEX_LAB_HOME='$LAB_HOME'
DEFAULT_CODEX_SHARED_STATE_HOME='$SHARED_STATE_HOME'
CODEX_LAB_INSTALL_ROOT=\${CODEX_LAB_INSTALL_ROOT:-'$INSTALL_ROOT'}
CODEX_LAB_ENTRYPOINT=\${CODEX_LAB_ENTRYPOINT:-'$ENTRYPOINT_RELATIVE'}

export CODEX_HOME=\${CODEX_LAB_HOME:-\$DEFAULT_CODEX_LAB_HOME}
export CODEX_SQLITE_HOME=\${CODEX_SHARED_STATE_HOME:-\$DEFAULT_CODEX_SHARED_STATE_HOME}
export CODEX_PREFER_BUNDLED_BWRAP=1
exec "\$CODEX_LAB_INSTALL_ROOT/current/\$CODEX_LAB_ENTRYPOINT" "\$@"
EOF
  chmod 0755 "$staged_launcher"

  if [ -e "$launcher" ] || [ -L "$launcher" ]; then
    if cmp -s "$launcher" "$staged_launcher"; then
      rm -f "$staged_launcher"
      return
    fi
    cp -p "$launcher" "$launcher.bak-$backup_stamp"
  fi
  mv -f "$staged_launcher" "$launcher"
}

maybe_strip_binary() {
  path="$1"
  is_true "$STRIP_BINARY" || return 0
  command -v strip >/dev/null 2>&1 || return 0

  case "$(uname -s)" in
    Darwin) strip -x "$path" 2>/dev/null || warn "Could not strip $path" ;;
    *) strip "$path" 2>/dev/null || warn "Could not strip $path" ;;
  esac
}

warn_if_linux_sandbox_unavailable() {
  [ "$(uname -s 2>/dev/null || true)" = Linux ] || return 0
  bundled_bwrap="$RELEASE_DIR/codex-resources/bwrap"
  if ! "$bundled_bwrap" --unshare-user --unshare-pid --proc /proc --dev /dev --ro-bind / / -- /bin/true >/dev/null 2>&1; then
    warn "The bundled bubblewrap cannot create the required namespaces; read-only explorer/reviewer agents will be refused. Enable unprivileged user namespaces according to your system policy, then run: codex-lab doctor"
  fi
}

install_release_package() {
  case "$TARGET" in
    x86_64-unknown-linux-musl | aarch64-unknown-linux-musl | \
      x86_64-apple-darwin | aarch64-apple-darwin | \
      x86_64-pc-windows-msvc | aarch64-pc-windows-msvc)
      ;;
    *)
      echo "Unsupported Codex Lab release target: $TARGET" >&2
      exit 1
      ;;
  esac

  package_asset="codex-package-$TARGET.tar.gz"
  checksum_asset=codex-package_SHA256SUMS
  package_url="$GITHUB_DOWNLOAD_BASE/$RELEASE_TAG/$package_asset"
  checksum_url="$GITHUB_DOWNLOAD_BASE/$RELEASE_TAG/$checksum_asset"

  mkdir -p "$RELEASES_DIR"
  package_path="$TMP_DIR/$package_asset"
  checksum_path="$TMP_DIR/$checksum_asset"
  step "Downloading Codex Lab $RELEASE_VERSION for $TARGET"
  download_file "$checksum_url" "$checksum_path"
  metadata_digest=$(printf '%s\n' "$release_metadata" | json_asset_digest "$checksum_asset")
  if [ -n "$metadata_digest" ]; then
    case "$metadata_digest" in
      sha256:*) verify_file_digest "$checksum_path" "${metadata_digest#sha256:}" ;;
      *)
        echo "Invalid SHA-256 digest for $checksum_asset in release metadata." >&2
        exit 1
        ;;
    esac
  fi
  expected_digest=$(manifest_digest "$checksum_path" "$package_asset") || {
    echo "Could not find SHA-256 digest for $package_asset in $checksum_asset." >&2
    exit 1
  }
  download_file "$package_url" "$package_path"
  verify_file_digest "$package_path" "$expected_digest"
  metadata_digest=$(printf '%s\n' "$release_metadata" | json_asset_digest "$package_asset")
  if [ -n "$metadata_digest" ]; then
    case "$metadata_digest" in
      sha256:*) verify_file_digest "$package_path" "${metadata_digest#sha256:}" ;;
      *)
        echo "Invalid SHA-256 digest for $package_asset in release metadata." >&2
        exit 1
        ;;
    esac
  fi

  case "$TARGET" in
    *windows*)
      ENTRYPOINT_RELATIVE=bin/codex.exe
      ;;
    *)
      ENTRYPOINT_RELATIVE=bin/codex
      ;;
  esac

  rm -rf "$STAGING_DIR"
  mkdir -p "$STAGING_DIR"
  tar -xzf "$package_path" -C "$STAGING_DIR"
  [ -f "$STAGING_DIR/codex-package.json" ] || {
    echo "Codex Lab package is missing codex-package.json." >&2
    exit 1
  }
  [ -x "$STAGING_DIR/$ENTRYPOINT_RELATIVE" ] || {
    echo "Codex Lab package is missing executable $ENTRYPOINT_RELATIVE." >&2
    exit 1
  }
  [ -d "$STAGING_DIR/codex-path" ] && [ -d "$STAGING_DIR/codex-resources" ] || {
    echo "Codex Lab package does not use the official package layout." >&2
    exit 1
  }

  step "Installing versioned release $RELEASE_ID"
  if [ -e "$RELEASE_DIR" ]; then
    existing_entrypoint="$RELEASE_DIR/$ENTRYPOINT_RELATIVE"
    existing_sha256=$(sha256_file "$existing_entrypoint")
    staged_sha256=$(sha256_file "$STAGING_DIR/$ENTRYPOINT_RELATIVE")
    if [ "$existing_sha256" != "$staged_sha256" ]; then
      echo "Release $RELEASE_ID already exists with a different binary." >&2
      exit 1
    fi
    rm -rf "$STAGING_DIR"
  else
    mv "$STAGING_DIR" "$RELEASE_DIR"
  fi
}

parse_args "$@"
[ -n "$SOURCE_BINARY" ] && RELEASE_MODE=false

if [ "$RELEASE_MODE" = true ]; then
  resolve_release
  resolve_host_target
  RELEASE_ID="${RELEASE_ID:-$RELEASE_VERSION-$TARGET}"
else
  resolve_bwrap_source

  if [ -n "$SOURCE_BWRAP" ]; then
    BWRAP_SHA256=$(sha256_file "$SOURCE_BWRAP")
    if [ "$BWRAP_SHA256" = unavailable ]; then
      echo "sha256sum or shasum is required to verify the bundled bwrap." >&2
      exit 1
    fi
  else
    BWRAP_SOURCE_KIND=not-required
    BWRAP_SHA256=not-required
  fi

  if [ -z "$SOURCE_BINARY" ]; then
    command -v cargo >/dev/null 2>&1 || {
      echo "cargo is required unless --binary is provided." >&2
      exit 1
    }
    step "Building codex-cli in release mode"
    if [ "$BWRAP_SHA256" = not-required ]; then
      (cd "$CODEX_RS_DIR" && cargo build --release -p codex-cli)
    else
      (cd "$CODEX_RS_DIR" && CODEX_BWRAP_SHA256="$BWRAP_SHA256" cargo build --release -p codex-cli)
    fi
    SOURCE_BINARY="$CODEX_RS_DIR/target/release/codex"
  fi

  [ -f "$SOURCE_BINARY" ] && [ -x "$SOURCE_BINARY" ] || {
    echo "Codex binary is not executable: $SOURCE_BINARY" >&2
    exit 1
  }

  if [ "$(uname -s 2>/dev/null || true)" = Linux ]; then
    verify_codex_bwrap_digest "$SOURCE_BINARY"
  fi

  if [ -z "$RELEASE_ID" ]; then
    RELEASE_ID=$(default_release_id)
  fi
  case "$(uname -s 2>/dev/null || true)" in
    MINGW* | MSYS* | CYGWIN*) ENTRYPOINT_RELATIVE=bin/codex.exe ;;
    *) ENTRYPOINT_RELATIVE=bin/codex ;;
  esac
fi

validate_release_id "$RELEASE_ID"

RELEASES_DIR="$INSTALL_ROOT/releases"
RELEASE_DIR="$RELEASES_DIR/$RELEASE_ID"
STAGING_DIR="$RELEASES_DIR/.staging.$RELEASE_ID.$$"
CURRENT_LINK="$INSTALL_ROOT/current"
CONFIG_TMP="$LAB_HOME/.config.toml.$$"
TMP_DIR=$(mktemp -d)

cleanup() {
  rm -rf "$STAGING_DIR"
  rm -f "$CONFIG_TMP"
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$LAB_HOME" "$SHARED_STATE_HOME"
ensure_multi_agent_namespace

step "Installing versioned release $RELEASE_ID"
mkdir -p "$RELEASES_DIR"
if [ "$RELEASE_MODE" = true ]; then
  install_release_package
else
  rm -rf "$STAGING_DIR"
  mkdir -p "$STAGING_DIR/bin"
  cp "$SOURCE_BINARY" "$STAGING_DIR/$ENTRYPOINT_RELATIVE"
  chmod 0755 "$STAGING_DIR/$ENTRYPOINT_RELATIVE"
  maybe_strip_binary "$STAGING_DIR/$ENTRYPOINT_RELATIVE"

  if [ -n "$SOURCE_BWRAP" ]; then
    verify_codex_bwrap_digest "$STAGING_DIR/$ENTRYPOINT_RELATIVE"
    mkdir -p "$STAGING_DIR/codex-resources"
    cp "$SOURCE_BWRAP" "$STAGING_DIR/codex-resources/bwrap"
    chmod 0755 "$STAGING_DIR/codex-resources/bwrap"
    staged_bwrap_sha256=$(sha256_file "$STAGING_DIR/codex-resources/bwrap")
    if [ "$staged_bwrap_sha256" != "$BWRAP_SHA256" ]; then
      echo "Bundled bwrap changed while it was being installed." >&2
      exit 1
    fi
  fi

  binary_sha256=$(sha256_file "$STAGING_DIR/$ENTRYPOINT_RELATIVE")
  cat >"$STAGING_DIR/manifest.txt" <<EOF
release_id=$RELEASE_ID
source_binary=$SOURCE_BINARY
binary_sha256=$binary_sha256
bwrap_source=$SOURCE_BWRAP
bwrap_source_kind=$BWRAP_SOURCE_KIND
bwrap_sha256=$BWRAP_SHA256
built_at=$(date '+%Y-%m-%dT%H:%M:%S%z')
EOF
fi

if [ "$RELEASE_MODE" != true ]; then
  if [ -e "$RELEASE_DIR" ]; then
    existing_sha256=$(sha256_file "$RELEASE_DIR/$ENTRYPOINT_RELATIVE")
    if [ "$existing_sha256" != "$binary_sha256" ]; then
      echo "Release $RELEASE_ID already exists with a different binary." >&2
      exit 1
    fi
    if [ -n "$SOURCE_BWRAP" ]; then
      existing_bwrap="$RELEASE_DIR/codex-resources/bwrap"
      if [ ! -x "$existing_bwrap" ] || [ "$(sha256_file "$existing_bwrap")" != "$BWRAP_SHA256" ]; then
        echo "Release $RELEASE_ID already exists with a different bundled bwrap." >&2
        exit 1
      fi
    fi
    rm -rf "$STAGING_DIR"
  else
    mv "$STAGING_DIR" "$RELEASE_DIR"
  fi
fi

replace_path_with_symlink "$CURRENT_LINK" "releases/$RELEASE_ID" "$INSTALL_ROOT/.current.$$"

step "Configuring shared Codex home"
ensure_shared_rollout_dir sessions
ensure_shared_rollout_dir archived_sessions
install_launcher
warn_if_linux_sandbox_unavailable

if is_true "$RUN_DOCTOR"; then
  step "Validating codex-lab"
  if ! "$BIN_DIR/codex-lab" doctor --summary --no-color --ascii; then
    warn "codex-lab was installed, but doctor reported a problem"
  fi
fi

cat <<EOF

codex-lab installed successfully.
  launcher:      $BIN_DIR/codex-lab
  release:       $RELEASE_DIR
  lab config:    $LAB_HOME
  shared state:  $SHARED_STATE_HOME

Use 'codex-lab resume --all' to see conversations from both Codex installations.
EOF
