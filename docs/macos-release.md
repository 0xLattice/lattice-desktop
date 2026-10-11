# macOS direct download

Lattice targets Apple Silicon (arm64), macOS 15 or newer. This distribution
uses ad-hoc code signatures. It is not Developer ID signed or notarized, and
Apple has not verified its publisher. Ad-hoc signatures check bundle integrity;
they do not authenticate the project. SHA256 checksums alone likewise do not
prove who published an archive.

## Verify and install

Obtain the versioned ZIP, `SHA256SUMS`, and `files.json` from the same release.
Before installation, compare the checksum manifest with an independently
trusted project signature or authenticated checksum channel. A project signing
key and its distribution channel must be approved before signed release
manifests are available; do not treat an unsigned candidate as authenticated.

```sh
# Once an approved public key and manifest signature are available:
minisign -Vm SHA256SUMS -p /path/to/trusted-lattice-releases.pub
shasum -a 256 -c SHA256SUMS
python3 scripts/release-macos.py verify /path/to/download-directory
```

After verifying the download, extract the ZIP using Archive Utility and move
`Lattice.app` to Applications. Check the extracted bundle if developer tools
are installed:

```sh
codesign --verify --deep --strict --verbose=2 /Applications/Lattice.app
```

Try opening Lattice. If macOS blocks an unidentified developer and you trust
the verified download, use **System Settings → Privacy & Security → Open
Anyway** for Lattice, then confirm **Open**. This creates an exception for this
app. Follow [Apple's current instructions](https://support.apple.com/en-us/102445).
Do not override malware or damaged-app warnings; verify the download and report
the failure. Managed Macs may disallow exceptions.

## Build the candidate

Use an Apple Silicon Mac with Xcode Command Line Tools, Swift, Rust/Cargo,
Node.js/npm and Python 3. Record exact versions (`xcodebuild -version`,
`swift --version`, `rustc -vV`, `cargo --version`, `node --version`,
`npm --version`, `python3 --version`, `sw_vers`) in private build evidence,
reviewing it before publication. Toolchains are not yet pinned by this project;
matching dependency locks alone is not a bit-for-bit reproducibility guarantee.

```sh
git clone --recurse-submodules https://github.com/adalinxx/lattice-desktop.git
cd lattice-desktop
git checkout --detach <reviewed-release-commit>
git submodule update --init --recursive
npm ci
LATTICE_NODE_SOURCE=/tmp/lattice-release-node scripts/release-macos.sh ../candidate-a
python3 scripts/test-release-macos.py
npm test
npm --prefix vendor/nexus-wallet test
npm --prefix vendor/nexus-wallet run typecheck
cargo test --locked --manifest-path src-tauri/Cargo.toml
LATTICE_NODE_BIN=/tmp/lattice-release-node/.build/debug \
LATTICE_APP="$PWD/src-tauri/target/release/bundle/macos/Lattice.app" \
LATTICE_GPU_MINER="$PWD/.build-miner-gpu/target/release/lattice-miner-gpu" npm test
```

The final test command uses isolated roots and no peers; do not set
`LATTICE_LIVE`. Tests may mine private test chains and create simulated
transactions; they must not use real wallets or a live chain. Review the real
window separately with the same isolated configuration. Never launch the app
against an existing production profile for release testing.

The script builds the node and CLI from `NODE_COMMIT`, the GPU worker from
`MINER_GPU_COMMIT`, and the wallet/SDK from recursive submodule commits.
Swift stays unoptimized (`debug`): this preserves the existing workaround for
the Swift optimizer issue described in the README. Release node builds use a neutral temporary source path because SwiftPM resource accessors and some runtime diagnostics contain literal paths. The absolute `/tmp/lattice-release-node` path is a required build input: Swift runtime file macros are not changed by debug-prefix maps. Release builds serialize access to this root. For an independent repeat, preserve the first outputs, move this task-created source checkout aside, then let the builder create a fresh checkout at the same canonical path. Debug symbols are stripped
from staged helpers, and compiler paths are remapped. Rust dependencies use
`Cargo.lock`, Swift uses `Package.resolved`, and npm uses lockfiles. Helper
compatibility libraries and dependency resource bundles are included. Native binaries are signed individually before the enclosing app. No certificate,
notarization credential, or persistent signing key is created or used.

`release-macos.py` rejects unexpected executables, non-arm64 binaries,
non-system dynamic libraries, invalid or non-ad-hoc signatures, common home
paths, debug artifacts, and private-key markers. An optional
`LATTICE_AUDIT_DENYLIST` points to a private, newline-delimited identifier file;
never commit it or publish its findings. Automated scanning is only part of the
review: inspect asset metadata, visible artwork, embedded strings, plist data,
archive member names, third-party notices, and release prose. Preserve required
third-party copyright and license attribution.

## Measure reproducibility

Build again from an independent checkout at the same reviewed commit, with
fresh Swift and Cargo build directories and the same exact toolchain versions.
The desktop checkout path may differ; the helper source root must remain the canonical path specified above. The helpers use the linker’s reproducible mode while retaining UUID load commands, and the GPU worker strips debug metadata at link time before its UUID is computed; runtime sections are not removed or rewritten. Keep both signed app
bundles until comparison is complete:

```sh
python3 scripts/release-macos.py compare /path/to/first/Lattice.app /path/to/second/Lattice.app
cmp /path/to/candidate-a/Lattice-0.1.0-macos-arm64.zip /path/to/candidate-b/Lattice-0.1.0-macos-arm64.zip
```

The comparison hashes every file and compares permission modes, including
executables and signatures; it exits nonzero and lists changed relative paths
on mismatch. ZIP timestamps and permission metadata are canonicalized, but
that alone is not evidence of reproducible compilation. Publish the actual
comparison result with toolchain versions and source revisions. Until two
independent builds agree, describe the release as rebuildable from pinned
source, not proven bit-for-bit reproducible. Mach-O UUIDs, compiler output,
SDK versions, and signatures can differ; investigate rather than removing
runtime sections or hiding differences to obtain a match.

## Publication gate

Preparation never uploads a release. Before publication, verify clean source
and all source/submodule revisions, both commit attribution fields, release
text, archive contents, extracted signatures, UI behavior, and third-party
license completeness. Verify that every required source repository is
accessible to intended rebuilders; do not change repository visibility as part
of release preparation. Publish only reviewed files, not build logs, source
checkout metadata, local audit data, or raw tool output.

Proposed release authentication: Minisign detached signatures over
`SHA256SUMS`, using a dedicated encrypted offline Ed25519 project key and the
public identity `Lattice releases`. Keep the secret outside Git, build outputs,
and CI, with an offline encrypted backup. Enter a passphrase only through a
local secure prompt. Publish only the approved public key and its fingerprint
through an independently authenticated project channel. Creating/importing the
key, its storage arrangement, and publishing that public identity each require
maintainer approval. Until then, project-signature verification is a release
blocker; ad-hoc code signing is not its replacement.

The bundled Swift runtime license is from the [Swift 6.3.3 release](https://github.com/swiftlang/swift/blob/swift-6.3.3-RELEASE/LICENSE.txt). Additional upstream license sources are recorded in `licenses/sources.json`. Notice collection fails closed when an upstream dependency supplies no license text; resolve the source notice before producing a releasable package.
