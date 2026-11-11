# xiv-fuse

[![Build](https://img.shields.io/github/actions/workflow/status/WorkingRobot/xiv-fuse/build.yml?style=for-the-badge&label=Build
)](https://github.com/WorkingRobot/xiv-fuse/actions/workflows/build.yml)
[![License](https://img.shields.io/github/license/WorkingRobot/xiv-fuse?style=for-the-badge&)](/LICENSE)
[![FFXIV Version](https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fexd.camora.dev%2Fapi%2Fglobal%2Fversions&query=latest&style=for-the-badge&label=Latest%20XIV%20Version
)](https://thaliak.xiv.dev/repository/4e9a232b)

xiv-fuse mounts every Final Fantasy XIV game file, for every region and patch version, as a read-only directory tree. Nothing is downloaded up front: files are fetched from the CDN as you read them, reconstructed on the fly, and cached on disk. It's the same backend that powers [EXDViewer](https://github.com/WorkingRobot/EXDViewer), exposed as a filesystem instead of a web API.

## Features

- **Every Region and Version:** Browse global, korea, and china at `latest` or any historical patch, with no local game install and no launcher.
- **On-Demand:** Files stream from the CDN as you read them.
- **Point-in-Time Installs:** `--backfill` reconstructs a coherent install as it existed on any past patch date, expansions and all.
- **Faithful Layout:** A read-only reproduction of the real on-disk tree, safe to point [ironworks](https://github.com/ackwell/ironworks), [Lumina](https://github.com/NotAdam/Lumina), or maybe even the game itself (untested).
- **Scoped Mounts:** Restrict the tree to a single region or patch with `--region` and `--patch`.

## Quick Start

Build and mount:

```bash
git clone --recursive https://github.com/WorkingRobot/xiv-fuse.git
cd xiv-fuse
mkdir -p /tmp/xiv
cargo run --release -- /tmp/xiv
```

Give it a few seconds to pull the repository list from [Thaliak](https://thaliak.xiv.dev/), then browse it from another shell:

```bash
ls /tmp/xiv                                             # global korea china
ls /tmp/xiv/global/latest/game/sqpack/ffxiv             # 000000.win32.index, ...
cp /tmp/xiv/global/latest/game/sqpack/ffxiv/000000.win32.index .
```

Unmount when you're done:

```bash
fusermount3 -u /tmp/xiv
```

## Layout

```
<mountpoint>/
  global/                         # also: korea, china
    latest/                       # or a specific version, e.g. 2026.06.18.0000.0000
      boot/                       # global only
        ffxivboot.exe
        ...
      game/
        ffxiv_dx11.exe
        sqpack/
          ffxiv/
            000000.win32.index
            000000.win32.dat0
          ex1/  ex2/  ex3/  ex4/  ex5/
```

Each of `boot`, `game`, and `ex1`..`ex5` is a separate repository with its own version history, so `latest` gives you a coherent newest-of-everything install, while a specific version directory only contains the repos that shipped on exactly that date. Pass `--backfill` to instead fill each expansion and `boot` in to its newest version at or before that date, reconstructing the install as it existed at that point in time (an expansion that didn't exist yet is left out).

Historic patches (prefixed by an `H`) are hidden from listings by default to keep the tree tidy; they stay reachable by explicit path (`.../global/H2024.05.31.0000.0000p/...`). Pass `--history` to list them too.

## Usage

```
xiv-fuse [OPTIONS] <MOUNTPOINT>

  --region <REGION>    Only expose one region: global, korea, or china
  --patch <PATCH>      Only expose one patch: latest, or a version like 2026.06.18.0000.0000
  --history            Also list H-prefixed historic patches (hidden by default)
  --backfill           Backfill each expansion/boot repo to its newest version at or before --patch
  --cache-dir <DIR>    On-disk cache for downloaded patch data [default: ~/.cache/xiv-fuse]
  --cache-size <GIB>   Ceiling on that cache; sparse, not reserved [default: 10]
  --clut-cache <MIB>   RAM budget for parsed CLUTs [default: 512]
  --clut-path <URL>    Base URL to fetch .clut files from, as <base>/<repo>/<version>.clut
  --index-path <PATH>  Directory or base URL of the patch index [default: WorkingRobot/ffxiv-patches]
  -h, --help           Print help
  -V, --version        Print version
```

Run `xiv-fuse --help` for the full text of each option. For example, to expose only the current global client:

```bash
cargo run --release -- /tmp/xiv --region global --patch latest
```

Notes on the two cache flags, which are easy to mix up:

- `--cache-size` is a ceiling in **GiB** on downloaded patch data in `--cache-dir`. It's a ceiling rather than a reservation (the slab files are sparse), but a working set larger than it will evict and re-download, so size it to what you intend to read. The cache persists across mounts.
- `--clut-cache` is a RAM budget in **MiB** for parsed CLUTs. The base game's is the largest at roughly 7 MB; expansion repositories are a couple of MB each, and a small one is kilobytes. Below 32 MiB the budget can't hold the set backing a single version, so a read that moves between repositories re-fetches and re-parses constantly; xiv-fuse warns if you go under.

## What Is SqPack?

FFXIV ships its game data in an archive format called SqPack: paired `.index` and `.dat` files under `game/sqpack/<repo>/`, split into numbered categories (`0a0000` is Excel data, `020000` is bulk model and terrain data, and so on). Files inside are addressed by a hash of their path rather than stored under a name, which is why readers expect that exact on-disk layout rather than a plain directory of files. xiv-fuse reproduces this layout byte for byte on-demand.

More info is available [here](https://xiv.dev/game-data/file-formats/sqpack).

## What Is a CLUT?

Square Enix distributes updates as [patch files](https://xiv.dev/data-files/zipatch), consisting of a stream of instructions that mutate the SqPack archives in place. Getting a single file at a single version normally means downloading every patch since the beginning and applying them in order, which is hundreds of gigabytes to reach `latest`.

A CLUT is a lookup table, published per repository and version by [ffxiv-downloader](https://github.com/WorkingRobot/ffxiv-downloader) to [ffxiv-lut](https://github.com/WorkingRobot/ffxiv-lut), that collapses all of that into a direct map: for every file in the repository, the exact byte ranges of the exact patch files that produce its final contents. xiv-fuse fetches the CLUT for whichever version you browse, then issues HTTP range requests for only the ranges a read actually touches.

## Building from Source

Requirements: Linux with FUSE (`fusermount3`; `fuse3` package on most distros) and Rust.

1. Clone the repository:
    ```bash
    git clone --recursive https://github.com/WorkingRobot/xiv-fuse.git
    cd xiv-fuse
    ```

2. Build the project:
    ```bash
    cargo build --release
    ```

## Notes

- **Read-only:** the mount rejects all writes.
- **Cache:** downloaded patch data persists across mounts, so re-reads are fast and offline-friendly. Delete `--cache-dir` to reset.
- **Throughput:** cold reads are bound by CDN latency and rate limits; xiv-fuse reads ahead in large windows so sequential reads stay fast.

## Contributing

Contributions, bug reports, and feature requests are welcome! Please open an [issue](https://github.com/WorkingRobot/xiv-fuse/issues) or a [pull request](https://github.com/WorkingRobot/xiv-fuse/pulls).

## Credits

Built on [ffxiv-downloader](https://github.com/WorkingRobot/ffxiv-downloader), with patch metadata from [Thaliak](https://thaliak.xiv.dev/).
