# Graph Report - FS SpaceMaker  (2026-11-27)

## Corpus Check
- 38 files · ~43,519 words
- Verdict: corpus is large enough that graph structure adds value.
- Unclassified: 4 file(s) not represented in the graph (top: (none) 1, .code-workspace 1, .cmd 1)

## Summary
- 970 nodes · 2627 edges · 49 communities (45 shown, 4 thin omitted)
- Extraction: 87% EXTRACTED · 13% INFERRED · 0% AMBIGUOUS · INFERRED: 341 edges (avg confidence: 0.85)
- Token cost: 85,063 input · 0 output

## Graph Freshness
- Built from commit: `76956339`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- Paths, Tree Match, Targets
- Apply Output and Results
- CLI and Configuration
- Windows Integration Tests
- Platform Layer (NTFS, Handles)
- File Matching Modes
- SQLite Fingerprint Index
- Apply Safety Checks
- Report Data Builder
- Plan and Output Files
- Progress and Formatting
- VCS Metadata Detection
- Safety Tests
- Scan Pipeline
- Scan and Apply Tests
- Fingerprint Digests
- Merge Mode Tests
- Test Helpers and CLI Tests
- Folder Scan Tests
- BLAKE3 Hashing
- Action Statuses
- Guided Runner Script
- Plan Totals and Report Rows
- Inventory Walk
- Fingerprint Batching
- Planned Deletions and Moves
- Scan Report Cards
- Inventory Model
- README Workflow Concepts
- Scan Entry Points
- Residual Reasons
- Report Tables and Filters
- README Design Concepts
- Skipped Entries
- Stress Test
- VCS Metadata Removal Plan
- VCS Metadata Kinds
- Report Header and Cards
- Report Folder Tree
- Report Template and Docs
- Volume Space
- Card Tones
- Executable Entry Point
- Largest Files View
- Test Runner Script
- Stream Coverage Check
- graphify Rules
- Cargo Package

## God Nodes (most connected - your core abstractions)
1. `Inventory` - 45 edges
2. `display_path()` - 39 edges
3. `ActionStatus` - 35 edges
4. `MatchMode` - 35 edges
5. `run_apply()` - 33 edges
6. `ApplyOutput` - 32 edges
7. `Plan` - 30 edges
8. `run_scan()` - 30 edges
9. `Sandbox` - 25 edges
10. `ScanOutput` - 25 edges

## Surprising Connections (you probably didn't know these)
- `apply()` --references--> `ApplyOutput`  [INFERRED]
  tests/folders.rs → src/apply.rs
- `check_pairing()` --references--> `ScanOutput`  [INFERRED]
  tests/properties.rs → src/scan.rs
- `scanned_duplicate()` --references--> `ScanOutput`  [INFERRED]
  tests/safety.rs → src/scan.rs
- `check_pairing()` --calls--> `name_key()`  [INFERRED]
  tests/properties.rs → src/paths.rs
- `apply()` --calls--> `run_apply()`  [INFERRED]
  tests/folders.rs → src/apply.rs

## Import Cycles
- 2-file cycle: `src/matching.rs -> src/tree_match.rs -> src/matching.rs`
- 2-file cycle: `src/inventory.rs -> src/vcs.rs -> src/inventory.rs`
- 4-file cycle: `src/inventory.rs -> src/vcs.rs -> src/plan.rs -> src/matching.rs -> src/inventory.rs`
- 5-file cycle: `src/inventory.rs -> src/vcs.rs -> src/plan.rs -> src/matching.rs -> src/tree_match.rs -> src/inventory.rs`
- 5-file cycle: `src/fingerprint.rs -> src/inventory.rs -> src/vcs.rs -> src/plan.rs -> src/matching.rs -> src/fingerprint.rs`

## Hyperedges (group relationships)
- **Report page section renderers** — assets_report_renderheader, assets_report_rendernotes, assets_report_rendercards, assets_report_rendervolumes, assets_report_renderfolders, assets_report_renderlargest, assets_report_rendertable [EXTRACTED 1.00]
- **Scan, review, dry run, apply workflow** — readme_scan_command, readme_html_report, readme_dry_run, readme_apply_command, readme_confirmation_code [EXTRACTED 1.00]
- **Data safety guarantees** — readme_apply_verification, readme_folder_safeguards, readme_never_touched, readme_plan_identifier, readme_ntfs_ads [INFERRED 0.85]

## Communities (49 total, 4 thin omitted)

### Community 0 - "Paths, Tree Match, Targets"
Cohesion: 0.06
Nodes (61): FileMatch, RelocatedTree, accepts_ordinary_plan_paths(), detects_overlapping_roots(), display_joined_uses_native_separator(), drive_relative_paths_are_refused(), ensure_disjoint_roots(), ensure_not_drive_relative() (+53 more)

### Community 1 - "Apply Output and Results"
Cohesion: 0.09
Nodes (31): ActionResult, ApplyOutput, build_cards(), build_folders(), build_notes(), build_report(), build_source_volume(), build_tables() (+23 more)

### Community 2 - "CLI and Configuration"
Cohesion: 0.07
Nodes (40): ApplyArgs, Cli, Command, Apply, Scan, EXIT_PARTIAL_FAILURE, print_apply_summary(), print_scan_summary() (+32 more)

### Community 3 - "Windows Integration Tests"
Cohesion: 0.08
Nodes (40): display_path(), display_strips_verbatim_prefix(), admin_share_path(), administrative_share_alias_of_the_destination_is_refused(), administrative_share_works_as_a_normal_source(), DeleteDenied, destination_open_for_writing_keeps_the_source(), download_origin_mark_does_not_block_deletion() (+32 more)

### Community 4 - "Platform Layer (NTFS, Handles)"
Cohesion: 0.07
Nodes (41): bare_stream_name(), clear_read_only(), CLOUD_ATTRIBUTE_MASK, cloud_attributes_are_recognised(), copies_keep_the_modification_time_and_read_only_copies_can_be_removed(), copy_preserving_metadata(), CROSS_DEVICE_ERROR_CODE, delete_access_is_granted_on_ordinary_files() (+33 more)

### Community 5 - "File Matching Modes"
Cohesion: 0.09
Nodes (34): Fingerprints, FileRecord, bucket_key(), BucketKey, choose_destination(), collect_destinations(), collect_destinations_by_digest(), digest_of() (+26 more)

### Community 6 - "SQLite Fingerprint Index"
Cohesion: 0.08
Nodes (28): a_folder_reuses_rows_stored_for_its_drive(), BUSY_TIMEOUT, COUNT_RANGE_SQL, DELETE_SQL, HashIndex, INDEX_FILE_NAME, IndexedHashes, KEY_RANGE_END (+20 more)

### Community 7 - "Apply Safety Checks"
Cohesion: 0.21
Nodes (29): ActionContext, check_destination_handle(), check_destination_metadata(), check_destination_present(), check_identical_content(), check_not_same_file(), check_preconditions(), check_read_only_allowed() (+21 more)

### Community 8 - "Report Data Builder"
Cohesion: 0.11
Nodes (28): add_file(), aggregate_folders(), ancestors_of(), collect_folder_rels(), CSV_HEADER, CSV_UTF8_BOM, DATA_PLACEHOLDER, file() (+20 more)

### Community 9 - "Plan and Output Files"
Cohesion: 0.11
Nodes (27): APPLY_FILE_NAMES, apply_paths(), ApplyPaths, clear_previous_outputs(), clears_only_known_outputs(), dry_run_and_apply_use_different_files(), DRY_RUN_FILE_NAMES, PhaseFileNames (+19 more)

### Community 10 - "Progress and Formatting"
Cohesion: 0.11
Nodes (15): BYTE_UNITS, format_bytes(), format_duration(), formats_durations(), formats_large_sizes_with_units(), formats_small_sizes_exactly(), SECONDS_PER_HOUR, SECONDS_PER_MINUTE (+7 more)

### Community 11 - "VCS Metadata Detection"
Cohesion: 0.10
Nodes (24): REL_SEPARATOR, BAZAAR_NAMES, clear_read_only(), FILE_URI_SCHEME, GIT_NAMES, holds_only_removable_metadata(), MERCURIAL_NAMES, metadata_removal_handles_read_only_files() (+16 more)

### Community 12 - "Safety Tests"
Cohesion: 0.15
Nodes (23): ApplyRequest, output_dir_of(), run_apply(), apply_request(), overwrite_keeping_mtime(), apply_requires_the_matching_confirmation_code(), applying_twice_deletes_nothing_more(), both_sides_changed_identically_is_still_kept() (+15 more)

### Community 13 - "Scan Pipeline"
Cohesion: 0.18
Nodes (23): HashStats, MatchOutcome, canonical_root(), apply_safety_filters(), build_plan(), build_pool(), delete_access_rejection(), DIGEST_PREVIEW_CHARS (+15 more)

### Community 14 - "Scan and Apply Tests"
Cohesion: 0.10
Nodes (10): planned_sources(), content_mode_deletes_only_identical_files(), dry_run_verifies_but_deletes_nothing(), empty_files_are_excluded_unless_requested(), MULTI_BUFFER_BYTES, only_folders_emptied_by_deletions_are_removed(), prediction_matches_the_measured_result(), relocated_mode_deletes_whole_moved_folders() (+2 more)

### Community 15 - "Fingerprint Digests"
Cohesion: 0.20
Nodes (6): computes_partial_then_full_and_reuses_index(), DigestKind, Full, Partial, Fingerprints<'a>, HashingRuntime

### Community 16 - "Merge Mode Tests"
Cohesion: 0.18
Nodes (22): folders(), a_destination_path_taken_after_the_scan_is_never_replaced(), a_file_in_use_is_skipped_and_kept(), a_source_changed_after_the_scan_is_neither_moved_nor_deleted(), a_source_folder_blocked_by_a_destination_file_is_renamed_whole(), applying_twice_does_nothing_more(), dry_run_changes_nothing(), empty_files_are_merged_too() (+14 more)

### Community 17 - "Test Helpers and CLI Tests"
Cohesion: 0.15
Nodes (10): full_workflow_through_the_executable(), help_lists_both_commands(), invalid_configuration_is_rejected(), invalid_mode_is_rejected(), overlapping_roots_fail_with_a_clear_message(), program(), scan_through_cli(), native() (+2 more)

### Community 18 - "Folder Scan Tests"
Cohesion: 0.30
Nodes (17): write_file(), a_folder_scan_reuses_the_fingerprints_of_a_parent_scan(), apply(), destination_metadata_is_never_used_as_a_copy(), entries(), files_whose_only_copy_is_in_a_trash_are_kept(), git_repositories_are_never_deleted_even_when_fully_copied(), make_svn_working_copy() (+9 more)

### Community 19 - "BLAKE3 Hashing"
Cohesion: 0.21
Nodes (15): ABC_DIGEST_HEX, compare_files(), compare_readers(), comparison_detects_every_difference(), comparison_digest_equals_file_digest(), Digest, empty_digest(), empty_file_has_empty_digest() (+7 more)

### Community 20 - "Action Statuses"
Cohesion: 0.11
Nodes (18): ActionStatus, AccessDenied, AlternateStreams, CloudPlaceholder, ContentDiffers, Deleted, DestinationChanged, DestinationMissing (+10 more)

### Community 21 - "Guided Runner Script"
Cohesion: 0.30
Nodes (16): Build-Executable(), Confirm-Choice(), ConvertTo-FolderPath(), Get-ScanArguments(), Invoke-Apply(), Invoke-DryRun(), Invoke-Scan(), Main() (+8 more)

### Community 22 - "Plan Totals and Report Rows"
Cohesion: 0.20
Nodes (12): display_joined(), Plan, ReportRow, apply_command_for(), build_tables(), deletion_rows(), error_rows(), move_rows() (+4 more)

### Community 23 - "Inventory Walk"
Cohesion: 0.21
Nodes (11): excluded_names_and_paths_are_not_walked(), is_excluded(), modification_time_is_preserved_in_nanoseconds(), modified_ns(), name_patterns_exclude_trash_folders(), records_files_and_dirs_in_pre_order(), scan_tree(), version_control_folders_are_measured_not_inventoried() (+3 more)

### Community 24 - "Fingerprint Batching"
Cohesion: 0.20
Nodes (9): batches_respect_the_file_limit(), CHECKPOINT_BYTES, CHECKPOINT_FILES, every_batch_is_saved_to_the_index(), runtime_parts(), SMALL_PREFIX, stale_index_rows_are_ignored(), TEST_ROOT (+1 more)

### Community 25 - "Planned Deletions and Moves"
Cohesion: 0.13
Nodes (3): PlannedDeletion, PlannedFile, PlannedMove

### Community 26 - "Scan Report Cards"
Cohesion: 0.24
Nodes (12): timestamp_iso(), Card, VolumeBar, build_cards(), build_notes(), build_report(), build_volumes(), deletion_check_note() (+4 more)

### Community 27 - "Inventory Model"
Cohesion: 0.26
Nodes (7): Inventory, measure_metadata(), MetadataDir, record_entry(), record_plain_entry(), rel_to_path(), describe_metadata()

### Community 28 - "README Workflow Concepts"
Cohesion: 0.18
Nodes (12): apply command (processing), apply-journal.csv, Options and Configuration File, Dry Run, Folder Safeguards (overlap and physical identity), FS SpaceMaker, Guided run (run-spacemaker.cmd / .ps1), Never-touched Items (links, cloud placeholders, trash, system files) (+4 more)

### Community 29 - "Scan Entry Points"
Cohesion: 0.24
Nodes (10): Config, run_scan(), ScanOutput, test_config(), check_soundness(), refuses_a_reports_folder_inside_the_source(), refuses_a_work_folder_inside_the_source(), refuses_identical_or_nested_roots() (+2 more)

### Community 30 - "Residual Reasons"
Cohesion: 0.17
Nodes (11): ResidualReason, AccessDenied, AlternateStreams, ContentDiffers, EmptyFileExcluded, NoCandidate, NotInMatchedTree, ReadOnly (+3 more)

### Community 31 - "Report Tables and Filters"
Cohesion: 0.22
Nodes (6): renderTable(), renderTabs(), tableCell(), tagClass(), Pre-deletion Verification Checks, Processing Statuses

### Community 32 - "README Design Concepts"
Cohesion: 0.28
Nodes (9): SQLite Fingerprint Index (index.sqlite), Three-stage Identity Filter (size, partial, full BLAKE3), Installation Folder and Single Output Set, Matching Modes (content / same-path / relocated), Merge Mode, NTFS Alternate Data Streams Handling, Relocated Mode, Test Suites (187 tests, run-tests.ps1) (+1 more)

### Community 33 - "Skipped Entries"
Cohesion: 0.25
Nodes (6): SkippedEntry, SkipReason, CloudPlaceholder, Link, NonUnicodeName, SpecialFile

### Community 34 - "Stress Test"
Cohesion: 0.33
Nodes (5): snapshot(), FILE_COUNT, many_files_with_collisions(), SIZE_CYCLE, UNIQUE_EVERY

### Community 35 - "VCS Metadata Removal Plan"
Cohesion: 0.33
Nodes (6): ancestors_are_inserted_once(), blocked_dirs(), display_paths_convert_only_below_the_root(), insert_with_ancestors(), plan_metadata_removals(), rel_of_display()

### Community 36 - "VCS Metadata Kinds"
Cohesion: 0.29
Nodes (5): MetadataKind, Bazaar, Git, Mercurial, Subversion

### Community 37 - "Report Header and Cards"
Cohesion: 0.40
Nodes (4): element(), formatBytes(), renderCards(), renderVolumes()

### Community 38 - "Report Folder Tree"
Cohesion: 0.40
Nodes (4): folderRow(), renderFolders(), setView() Before/After switch, treeRows()

### Community 39 - "Report Template and Docs"
Cohesion: 0.33
Nodes (6): report-data JSON placeholder, FS SpaceMaker Report Template, Architecture Module Map, Freed Space Prediction, The HTML Report, Residual Reason Tags

### Community 40 - "Volume Space"
Cohesion: 0.47
Nodes (3): reports_space_of_temp_volume(), volume_space(), VolumeSpace

### Community 41 - "Card Tones"
Cohesion: 0.33
Nodes (6): Tone, Bad, Good, Neutral, Warn, card()

### Community 43 - "Largest Files View"
Cohesion: 0.50
Nodes (3): folderPath(), joinPath(), renderLargest()

### Community 44 - "Test Runner Script"
Cohesion: 0.83
Nodes (3): Invoke-CargoStep(), Main(), Test-SymbolicLinkPrivilege()

### Community 45 - "Stream Coverage Check"
Cohesion: 0.67
Nodes (4): check_streams(), StreamCheck, Covered, Uncovered

## Knowledge Gaps
- **66 isolated node(s):** `Unconstrained`, `Failed`, `Kept`, `Removed`, `WouldRemove` (+61 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 252 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **4 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `MatchMode` connect `File Matching Modes` to `CLI and Configuration`, `Stress Test`, `Windows Integration Tests`, `Apply Safety Checks`, `Report Data Builder`, `Plan and Output Files`, `Safety Tests`, `Scan Pipeline`, `Scan and Apply Tests`, `Merge Mode Tests`, `Test Helpers and CLI Tests`, `Folder Scan Tests`, `Plan Totals and Report Rows`, `Scan Report Cards`, `Scan Entry Points`?**
  _High betweenness centrality (0.070) - this node is a cross-community bridge._
- **Are the 31 inferred relationships involving `display_path()` (e.g. with `build_notes()` and `build_report()`) actually correct?**
  _`display_path()` has 31 INFERRED edges - model-reasoned connections that need verification._
- **What connects `Unconstrained`, `Failed`, `Kept` to the rest of the system?**
  _66 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Paths, Tree Match, Targets` be split into smaller, more focused modules?**
  _Cohesion score 0.061088485746019994 - nodes in this community are weakly interconnected._
- **Why does `ActionStatus` connect `Action Statuses` to `Apply Output and Results`, `CLI and Configuration`, `Stress Test`, `Windows Integration Tests`, `Apply Safety Checks`, `Safety Tests`, `Scan and Apply Tests`, `Merge Mode Tests`?**
  _High betweenness centrality (0.050) - this node is a cross-community bridge._
- **Are the 16 inferred relationships involving `run_apply()` (e.g. with `format_bytes()` and `apply_paths()`) actually correct?**
  _`run_apply()` has 16 INFERRED edges - model-reasoned connections that need verification._
- **Should `Apply Output and Results` be split into smaller, more focused modules?**
  _Cohesion score 0.08672699849170437 - nodes in this community are weakly interconnected._