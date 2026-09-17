# Disk Usage Map 1.0

**English** | [한국어](README_kr.md)

A Windows disk space analyzer built with Rust and egui/eframe. Explore folder sizes in a hierarchical treemap and find large, old directories worth reviewing. The app does not delete files.

## Build and run

Install the Rust MSVC toolchain and the **Desktop development with C++** workload in Visual Studio Build Tools on Windows.

```powershell
cargo build --release
cargo run --release
# Choose an initial path without automatically starting a scan
cargo run --release -- "D:\"
# Show sample data without scanning a drive
cargo run --release -- --demo
```

The executable is `target/release/diskusagemap.exe`. Choose a drive or enter a folder path, then click **Start scan**. Use **Refresh drives** after connecting a removable drive.

## Automated releases (x64 / ARM64)

Push a version tag matching `Cargo.toml` to publish a GitHub Release:

```powershell
git tag -a v1.0.0 -m "버전 1.0.0 배포"
git push origin main
git push origin v1.0.0
```

The `Windows release` workflow builds and tests on native Windows x64 and ARM64 runners. Both must succeed before publication. Releases contain `diskusagemap-v<VERSION>-windows-x64.zip`, `diskusagemap-v<VERSION>-windows-arm64.zip`, and `SHA256SUMS.txt`. Each ZIP includes the executable and both READMEs; the MSVC runtime is linked statically. Choose the archive matching your Windows architecture.

Prerelease tags such as `v1.1.0-rc.1` must also match the package version. Published releases are not overwritten. Failed uploads leave a draft that can be retried. No additional secret is needed: publishing uses `GITHUB_TOKEN` with `contents: write`.

For verification without publishing, run **Actions → Windows release → Run workflow** on `main`. ZIPs remain as workflow artifacts for 14 days. Manual runs on version tags publish releases. For the next release, update `Cargo.toml`, regenerate `Cargo.lock`, commit, and tag that commit.

## Language

Choose **한국어** or **English** in the **Language / 언어** selector at the top of the window. Changes apply immediately without restarting or rescanning. Korean is the default; your selection is stored in `%LOCALAPPDATA%\DiskUsageMap\language.txt` and restored on the next launch.

Controls, tooltips, age labels, and status messages are localized. File and folder names and paths remain unchanged. Native operating-system error details may use the Windows language. Existing scan databases work with either UI language.

## Restore saved maps at startup

Completed scans automatically save folder sizes, timestamps, hierarchy, and incomplete flags to a **SQLite database**. The next launch restores a saved map instead of scanning the entire drive again.

- Location: `%LOCALAPPDATA%\DiskUsageMap\snapshots.sqlite3`, separate for each Windows user. SQLite is bundled; no database installation is required.
- The latest result is retained for each scan root. An explicit command-line path loads that root's saved result; otherwise, the most recently saved map opens.
- Enter a path and click **Open saved map** to restore it, even if the original drive is disconnected.
- The save time is displayed in local time. Changes after that time require **Refresh selected**, **Rescan current folder**, or **Rescan all**. Automatic filesystem monitoring and USN-based incremental updates are not implemented.
- Stopped, failed, in-progress, and demo scans are not saved. Completed scans with read errors are saved with incomplete flags.
- Loading and saving run in the background. Normal shutdown waits for pending saves. A save failure is reported and leaves the app open.
- Transactions preserve previously committed results if saving fails or the process exits abruptly. Corrupt or incompatible databases are reported instead of overwritten.
- The database contains folder paths, aggregate metadata, and diagnostics, not file contents or raw MFT records. It restores the map, not a per-file MFT index.
- To discard saved maps, close the app and delete the database file. Rename an incompatible database before creating a new one.

Restore time depends on the number of saved folders and storage speed. An initial scan is still required.

## Refresh folders after changes in Explorer

You can refresh a top-level folder without entering it. After emptying the Recycle Bin, select `$Recycle.Bin` in the root view and click **Refresh selected**. The view stays at the root. Zero-size folders remain selectable in the list.

- **Single-click** a tile or list entry to select it; **double-click** to enter the folder. The list also has an **Open** button. Selected tiles have an outline.
- Right-click a tile or list entry and choose **Refresh selected** to update it directly.
- Selection is tracked by path across updates and cleared if the selected folder is deleted.
- **Rescan current folder** updates the viewed folder and its descendants. Subfolder scans use directory enumeration.
- If the viewed folder was deleted, the app confirms its absence in an accessible parent's directory listing, removes it, and navigates to the nearest surviving ancestor. Multiple deleted ancestor levels are supported.
- Ancestor sizes and file counts are recomputed. Their direct-file metadata is reread so dates can decrease after deleting a newer child. Other sibling subtrees are not rescanned.
- The previous map remains visible during refresh. Read errors do not discard successful updates: unreadable portions retain previous totals and are marked incomplete, while readable folders' changes and deletions are applied. Partially readable listings retain old direct-file totals and unconfirmed old child folders.
- Stopping or failing to verify the root/drive path preserves the previous map and saved results. If the selected path becomes a file or link, rescan its parent.
- Changes are merged into the original full map and saved under the same root, so deleted items do not reappear after restarting.
- A partial-refresh note identifies the updated scope. Other folders still reflect earlier scans; the save time is not a full-drive scan time. Diagnostics are cumulative, and ancestor incomplete flags may remain conservative.
- Use **Rescan all** to account for moves outside the refreshed subtree, including files moved to the Recycle Bin.
- Refreshing a previously stopped map does not complete unscanned regions and is not automatically saved. Complete a full scan to obtain a saved result.

## Fast NTFS MFT scanning

Right-click the executable and choose **Run as administrator**, then select a drive root such as `C:\` or `D:\` to automatically try the MFT engine. Administrator privileges are optional; ordinary scans work without them.

- Local fixed/removable NTFS 3.0/3.1 volumes are supported. Folder and network paths use directory enumeration.
- The volume opens read-only. The MFT DATA runlist is resolved and read in batches of up to 4 MiB instead of querying each file. The app does not write to or lock the volume, or create a USN journal.
- Insufficient permissions, non-NTFS filesystems, unsupported layouts, invalid/changing records, or changes to MFT size/extents trigger fallback to directory enumeration. The status bar shows the engine and fallback reason.
- Progress measures MFT slots read, including unused slots. Records appear in previews as their parent references become available.
- Sector fixups are validated. Logical resident/nonresident DATA sizes and STANDARD_INFORMATION modification/access times are read. Extension records are merged only when base references and sequence numbers agree.
- Hard links are counted per path, as in directory enumeration. DOS 8.3 aliases are not counted again. Alternate data streams are excluded.
- Reserved NTFS metafiles (MFT records 0–15), entries under `$Extend`, and reparse points are excluded.
- Unverified extension references and uncommon nonresident ATTRIBUTE_LIST attributes on ordinary files mark affected folders incomplete. The MFT's own attribute list is limited to 16 MiB and 4,096 extension references; exceeding these limits triggers fallback.
- Administrator scans can reveal metadata unavailable through ordinary folder permissions. Memory scales with file records and names; millions of files can require hundreds of MiB or more.
- A live volume is not a consistent snapshot. Windows access-time limitations still apply. Administrator-mode compatibility and throughput require verification on the target environment.

Format references: [MFT record header](https://learn.microsoft.com/en-us/windows/win32/devnotes/file-record-segment-header), [attribute header and runlist](https://learn.microsoft.com/en-us/windows/win32/devnotes/attribute-record-header), [volume information](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-ntfs_volume_data_buffer).

## Explore the map

- Area represents logical file sizes. Colors range from recent (teal), through six months (yellow), to a year or more (red). Unknown dates are gray.
- Use **Up** and **Root** to navigate back. Display depth ranges from 1 to 4.
- Direct files share one tile. Each level displays the largest 100 folders and groups smaller ones under **Other folders**. The list provides access to all children.
- The right panel shows up to 200 old-folder candidates by size across the entire scan. Filter by age, minimum MiB, and path text. Parent and child candidates overlap; do not add their sizes together.
- Use **Copy path** and **Open in Explorer** to inspect folders yourself.
- Scanning runs on a worker thread. **Stop** displays results collected so far. Directory enumeration reports folder/file counts and bytes because the final total is unknown.
- Preview maps appear after roughly 250 ms once file sizes are available, then update about once per second. Large trees use longer intervals to limit snapshot overhead. Slow operating-system I/O can delay the first preview.
- Sizes and colors may change during scanning. Navigation is preserved, and old-folder candidates are finalized after completion.
- Directory enumeration reuses Windows directory-entry metadata and visits folders breadth-first to discover major branches early. See [Rust DirEntry metadata](https://doc.rust-lang.org/std/fs/struct.DirEntry.html#method.metadata).

## Dates and accounting limits

Age defaults to the **latest modification timestamp** across the folder itself and all descendant folders/files. Access-based age uses the **latest file access timestamp**; directory access times are excluded to reduce the effect of browsing. Missing dates make that age unknown. Future dates are shown as zero days ago.

Unmodified files can still be used frequently. Windows may disable or delay access-time updates, so timestamps are clues, not proof of usage. See [Microsoft file times](https://learn.microsoft.com/en-us/windows/win32/sysinfo/file-times).

- File contents are not analyzed. Raw MFT records may contain small files' resident data, but only size and metadata are used. Directory enumeration skips unreadable/vanished items and reports an error count with up to 100 details.
- Symbolic links, junctions, and other Windows reparse points are excluded to avoid cycles and crossing drives. This can include cloud placeholders.
- Incomplete folders, their ancestors, unknown dates, and stopped results are excluded from old-folder candidates.
- Logical lengths differ from actual allocation for compressed/sparse files. Hard links and system-reserved space can also make totals differ from Windows drive usage.
- Concurrent changes are not captured as a consistent snapshot. Directory enumeration retains folder metadata; MFT scanning also retains file records.
- Slow or unresponsive drive I/O can delay cancellation.

## Development checks

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
# Benchmark restoring 50,001 folders from a temporary database
cargo test --release benchmark_restore_50000_directories -- --ignored --nocapture
# Check engine selection and first-map timing without the GUI
cargo run --release -- --scan-check "C:\"
# Administrator PowerShell only: live MFT test without fallback
$env:DISKMAP_MFT_VOLUME = 'C:\'
cargo test --release live_mft_volume_scan -- --ignored --nocapture
```

| File | Responsibility |
| --- | --- |
| `src/app.rs` | Korean/English UI, navigation, filters, colors |
| `src/i18n.rs` | Language preference, translation helpers, diagnostic display |
| `src/scan.rs` | Cancellable traversal, size/date aggregation, drive discovery |
| `src/treemap.rs` | Area-proportional binary layout |
| `src/mft.rs` | NTFS parsing and reference-based aggregation |
| `src/mft/windows.rs` | Read-only volume access and batched MFT scanning |
| `src/cache.rs` | SQLite persistence, validation, background database worker |
| `src/subtree.rs` | Refresh, deletion checks, ancestor totals, subtree merging |
