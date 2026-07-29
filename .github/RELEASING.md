# Releasing agent-text

Release-plz prepares every automated release in a pull request. Merging that
generated pull request publishes the crate, creates a `vX.Y.Z` tag, and creates
the matching GitHub Release.

## Security invariants

- Automated crates.io publishing uses trusted publishing over OIDC.
- Never add a `CARGO_REGISTRY_TOKEN` repository or environment secret.
- Never add a crates.io authentication action to the release workflow.
- Only the release job receives `id-token: write`; the release-PR job does not.
- `release_always = false` prevents an ordinary push or manual version bump from
  publishing a crate.

## Bootstrap version 0.1.0

Crates.io cannot configure a trusted publisher until the crate exists. Complete
this one-time bootstrap from a clean `master` checkout before merging the pull
request that adds release automation:

```sh
git switch master
git pull --ff-only
git status --short
cargo publish --dry-run --locked
cargo publish --locked
```

Use a narrowly scoped crates.io token only on the maintainer's machine. Do not
put it in GitHub. After the publish succeeds, align the Git and GitHub release
surfaces with the published source:

```sh
git tag -a v0.1.0 -m "agent-text v0.1.0"
git push origin v0.1.0
gh release create v0.1.0 --verify-tag --title v0.1.0 --generate-notes
```

Then create an unprotected GitHub environment named `release` and add this
trusted publisher in the `agent-text` crate settings on crates.io:

| Field | Value |
| --- | --- |
| Provider | GitHub Actions |
| Owner | `getkono` |
| Repository | `agent-text` |
| Workflow | `release-plz.yml` |
| Environment | `release` |

Enable **Trusted Publishing Only** after saving the publisher. Remove the local
Cargo credential with `cargo logout` and revoke the one-time token on crates.io.

## Subsequent releases

1. Merge normal changes into `master`.
2. Review the `release-plz` pull request, including its version bump and
   generated `CHANGELOG.md`.
3. Merge the generated pull request.
4. Confirm that the `Release-plz release` job publishes through OIDC and creates
   the expected tag and GitHub Release.

Release-plz uses GitHub's ephemeral `GITHUB_TOKEN`. GitHub therefore does not
automatically start `pull_request` workflows for generated release pull
requests. Review the release-only changes directly and rely on the CI completed
by their source pull requests.

If publishing fails because the trusted-publisher fields do not match, correct
the GitHub environment or crates.io configuration and rerun the failed workflow.
Do not work around the failure by adding a registry token.
