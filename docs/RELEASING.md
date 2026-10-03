# Releasing Glide

Releases are built and published by the **Glide desktop** workflow (`.github/workflows/desktop.yml`).

1. Set the version everywhere and refresh the lock files:
   ```sh
   node scripts/set-version.js 0.3.0
   (cd core && cargo update --workspace) && (cd desktop/src-tauri && cargo update --workspace)
   ```
2. Commit, then tag and push: `git tag v0.3.0 && git push --tags`.
3. The workflow builds the Windows installer and the Apple Silicon app, signs and notarizes the Mac build, signs the
   update files and publishes a GitHub release with `latest.json`. Installed copies of Glide find it within a few
   hours, or at once with **Settings → Updates → Check for updates**.

## Repository secrets

| Secret | Used for |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Signing update files. The matching public key is in `desktop/src-tauri/tauri.conf.json`. Keep a backup: without it, installed copies cannot be updated. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Its password, if it has one. |
| `CSC_LINK`, `CSC_KEY_PASSWORD` | The Apple *Developer ID Application* certificate (`.p12`, base64) and its password. |
| `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`, `APPLE_TEAM_ID` | Notarizing the Mac build with Apple. |

Without the Apple secrets the Mac build is unsigned; without `TAURI_SIGNING_PRIVATE_KEY` the build fails, because
every release must carry signed update files.
