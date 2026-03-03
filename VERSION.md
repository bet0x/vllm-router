# Release Checklist

Follow these steps when preparing a new release.

## 1. Update the version

The version **must** be the same in all of the following places:

| File | Field |
|------|-------|
| `Cargo.toml` | `version = "X.Y.Z"` |
| Git tag | `vX.Y.Z` |

After editing `Cargo.toml`, run `cargo check` so that `Cargo.lock` is updated too.

## 2. Update the changelog

Add a new section to `CHANGELOG.md` with the version and date:

```markdown
## [X.Y.Z] — YYYY-MM-DD

### Added
- ...

### Changed
- ...

### Fixed
- ...
```

## 3. Commit, tag, and push

```bash
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: vX.Y.Z"
git tag vX.Y.Z
git push origin main --tags
```

## 4. What happens automatically

The GitHub Actions release workflow (`.github/workflows/release.yml`) triggers on `v*` tags and:

1. Builds the release binary (`cargo build --release`)
2. Packages a tarball with the binary, `configs/`, `docs/`, and `scripts/`
3. Creates a GitHub Release with auto-generated release notes
4. Builds and pushes the Docker image to `barrahome/vllm-router:<tag>` and `:latest`

## 5. Verify

- [ ] GitHub Release appears at https://github.com/bet0x/vllm-router/releases
- [ ] Tarball contains the binary, configs, docs, and scripts
- [ ] Docker image is available: `docker pull barrahome/vllm-router:vX.Y.Z`
