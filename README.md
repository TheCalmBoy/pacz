# pacz

`pacz` wraps pacman sync operations and downloads repository packages in parallel with
`aria2-next`. pacman still verifies packages and performs the install. `-Sw` stops after
downloads; packages without a SHA-256 in the sync database are handed back to pacman so
they can still be downloaded and verified.

The project targets Arch Linux and CachyOS. Package operations need root; `--dry-run`
prints the download plan without refreshing databases, downloading, or installing.

## Requirements

- Rust 1.74 or newer to build from source.
- pacman for package queries, downloads, and installation.
- `aria2-next` for parallel downloads (the default binary).
- `paru` is optional and is used for AUR updates after `-Su` / `-Syu`.

To use stock aria2 instead, configure `aria2 = aria2c` and `state_dir = none` in
`/etc/pacz.conf`.

## Build and check

```sh
cargo build --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

On Arch, `makepkg -si` builds and installs the package from this directory.

## Configuration

The optional configuration file is `/etc/pacz.conf`; `PACZ_CONFIG` selects another
path. See [`pacz.conf.example`](pacz.conf.example) for supported settings. pacz reads
pacman's `pacman.conf` to resolve repositories, mirror URLs, architecture, and cache
location.

Downloads first go to a staging directory. Completed files are moved into pacman's
cache only after aria2 reports success. A failed transaction does not start the pacman
install step.

## License

No license text was included in the source archive, so the PKGBUILD records the license
as unknown. Choose and add the project's license before publishing a release.
