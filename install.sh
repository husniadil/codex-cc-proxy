#!/bin/sh
#
# Install proxenos from a published release.
#
#   curl -fsSL https://raw.githubusercontent.com/husniadil/proxenos/main/install.sh | sh
#
# The checksum step is not optional and there is no flag to skip it. A script
# that fetches a binary over the network and runs it has exactly one defence,
# and an install that "worked" without taking it is the failure this exists to
# prevent. If no SHA-256 tool can be found, this refuses to install rather than
# continuing unverified.
#
# Nothing is written outside the install directory, and nothing is written at
# all until the archive has been verified.

set -eu

REPO="husniadil/proxenos"
BIN="proxenos"

VERSION="${PROXENOS_VERSION:-latest}"
BIN_DIR="${PROXENOS_BIN_DIR:-$HOME/.local/bin}"
TARGET="${PROXENOS_TARGET:-}"
DRY_RUN="${PROXENOS_DRY_RUN:-}"

usage() {
	cat <<EOF
Install $BIN.

  --version <tag>   a released tag, e.g. v0.1.1 (default: the latest release)
  --bin-dir <dir>   where to install (default: \$HOME/.local/bin)
  --target <triple> override platform detection
  --dry-run         report what would happen; download and install nothing
  --help

Each has an environment variable: PROXENOS_VERSION, _BIN_DIR, _TARGET,
_DRY_RUN.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--version)
		VERSION="${2:?--version needs a tag}"
		shift 2
		;;
	--bin-dir)
		BIN_DIR="${2:?--bin-dir needs a directory}"
		shift 2
		;;
	--target)
		TARGET="${2:?--target needs a triple}"
		shift 2
		;;
	--dry-run)
		DRY_RUN=1
		shift
		;;
	--help | -h)
		usage
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
done

die() {
	echo "error: $*" >&2
	exit 1
}

# The platform, as the release names it.
#
# An unrecognized platform is an error rather than a guess. Installing the
# wrong architecture produces a binary that fails at exec time with a message
# about the file rather than about the install, which is a worse place to learn
# it than here.
detect_target() {
	os="$(uname -s)"
	arch="$(uname -m)"

	case "$os" in
	Darwin)
		case "$arch" in
		arm64 | aarch64) echo "aarch64-apple-darwin" ;;
		x86_64) echo "x86_64-apple-darwin" ;;
		*) die "unsupported macOS architecture: $arch" ;;
		esac
		;;
	Linux)
		case "$arch" in
		x86_64 | amd64) echo "x86_64-unknown-linux-gnu" ;;
		aarch64 | arm64) echo "aarch64-unknown-linux-gnu" ;;
		*) die "unsupported Linux architecture: $arch" ;;
		esac
		;;
	MINGW* | MSYS* | CYGWIN*)
		die "Windows is released as a binary but not installed by this script.
Download proxenos-x86_64-pc-windows-msvc.tar.gz from
https://github.com/$REPO/releases and extract it where you want it."
		;;
	*)
		die "unsupported operating system: $os"
		;;
	esac
}

# One of the two SHA-256 tools, or nothing. `shasum` is macOS's, `sha256sum` is
# coreutils'; a machine with neither cannot verify, and cannot install.
sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "no sha256sum or shasum found, so the download cannot be verified.
Install one, or download the release manually and check it yourself:
https://github.com/$REPO/releases"
	fi
}

fetch() {
	# --fail so an HTML error page is never mistaken for an archive: without
	# it curl writes the 404 body to the file and exits 0.
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$1" -o "$2"
	elif command -v wget >/dev/null 2>&1; then
		wget -q "$1" -O "$2"
	else
		die "neither curl nor wget is available"
	fi
}

[ -n "$TARGET" ] || TARGET="$(detect_target)"

# Where the archives are fetched from. Overridable for a mirror, and for the
# one test that matters: proving a tampered archive is refused needs somewhere
# to serve a tampered archive from.
if [ -n "${PROXENOS_BASE_URL:-}" ]; then
	BASE="$PROXENOS_BASE_URL"
elif [ "$VERSION" = "latest" ]; then
	BASE="https://github.com/$REPO/releases/latest/download"
else
	BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

ARCHIVE="$BIN-$TARGET.tar.gz"

echo "$BIN"
echo "  version:  $VERSION"
echo "  platform: $TARGET"
echo "  into:     $BIN_DIR"

if [ -n "$DRY_RUN" ]; then
	echo "  would download $BASE/$ARCHIVE"
	echo "dry run: nothing downloaded, nothing installed."
	exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

echo "downloading..."
fetch "$BASE/$ARCHIVE" "$work/$ARCHIVE" ||
	die "could not download $BASE/$ARCHIVE
Check that the tag exists and that a binary is published for $TARGET."
fetch "$BASE/SHA256SUMS" "$work/SHA256SUMS" ||
	die "could not download the checksum file, so the archive cannot be verified"

echo "verifying..."
expected="$(grep " $ARCHIVE\$" "$work/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$expected" ] ||
	die "$ARCHIVE is not listed in SHA256SUMS for this release"

actual="$(sha256_of "$work/$ARCHIVE")"
[ "$actual" = "$expected" ] ||
	die "checksum mismatch for $ARCHIVE
  expected $expected
  got      $actual
Nothing has been installed. Do not use this download."

tar -xzf "$work/$ARCHIVE" -C "$work"
[ -f "$work/$BIN-$TARGET/$BIN" ] ||
	die "the archive does not contain $BIN where it was expected"

mkdir -p "$BIN_DIR"
install -m 755 "$work/$BIN-$TARGET/$BIN" "$BIN_DIR/$BIN" 2>/dev/null ||
	die "could not write to $BIN_DIR
Choose somewhere writable with --bin-dir, or create it yourself."

echo "installed $BIN_DIR/$BIN"
"$BIN_DIR/$BIN" --version

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*)
	echo
	echo "note: $BIN_DIR is not on your PATH. Add it:"
	echo "  export PATH=\"$BIN_DIR:\$PATH\""
	;;
esac
