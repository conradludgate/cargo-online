# cargo-online

Run the cargo-mutants catalog with shared Rust test builds. Cargo-mutants discovers the edits. Cargo-online compiles and executes them, including candidates that need a separate build.

## Run mutation tests

Install both commands from the cargo-online checkout:

```sh
cargo install --locked cargo-mutants
cargo install --locked --path .
```

From the target workspace:

```sh
cargo online mutate --test-workspace
cargo online mutate --test-workspace --workspace-coverage
cargo online mutate -p my-library --list
cargo online mutate -p my-library -j 4
cargo online mutate -p my-library --test-workspace --all-features
cargo online mutate -p my-library --lib
```

The default run uses the selected package's normal `cargo test` suite, including integration tests and doctests. From the workspace root, omit `-p` to discover candidates across all members. `--test-workspace` runs downstream workspace tests as well. Set Cargo features with `--features`, `--all-features`, or `--no-default-features`. Arguments after `--` go to cargo-mutants during discovery, for example `-- --file src/parser.rs`. Execution uses cargo-online's options and Cargo's native test runner.

Each candidate retains its upstream name, source span, and replacement. The engine batches up to 32 edits at a time, compiling alternative function bodies behind a per-process selector. It runs a clean baseline and also tests each runnable bundle with every mutation disabled. `-j` sets the maximum number of concurrent mutant test suites. Adjust `--batch-size` to control the size of each build.

For long runs, `--stream` prints each confirmed outcome to standard output in completion order. Redirect it to a file to retain completed results if the run stops early. A successful run still prints the totals at the end.

Constants, opaque return types, and other edits that require independent type checking use literal source builds. For each selected test suite the runner reads Cargo’s clean-build artifacts. A package emitted only as a test harness can use shared builds. For packages that also emit linkable artifacts it follows resolved dependencies from the build scripts and procedural macros present in that build. Packages used at build time, packages with an active build script or procedural macro, and cases with incomplete build evidence use literal builds. When rustc rejects a bundle, diagnostics identify variants to retry separately. The engine splits ambiguous failures into smaller batches and confirms viability with the literal edit before reporting **Unviable**. It reports every catalog candidate, including timeouts. Exit codes are 0 for a completed run containing only killed or unviable mutants, 1 for survivors, uncovered mutants, or timeouts, and 2 for a command, baseline, or infrastructure error.

For package-level library feedback, `--lib` records which unit tests entered each mutated function and runs those tests fastest first until a failure. **Uncovered** means no baseline unit test entered the function. `--lib --full` runs the full library unit suite for each mutant. `--test-workspace --lib` runs the workspace library suites for every mutant. By default literal retries run the selected Cargo suite. Package runs default to 120 seconds per suite (per test in coverage mode). With `--test-workspace`, the runner measures one clean test suite across the workspace, with a 30-minute default deadline, then allows each mutant suite twice that duration with a 120-second minimum. An explicit `--timeout` sets the limit for both clean and mutant tests. Builds have a separate `--build-timeout`, defaulting to 300 seconds.

With `--test-workspace --workspace-coverage`, the unmutated bundle runs the selected workspace suite and records which test executables enter each mutated function. A mutant with no observed entry is **Uncovered**. For a covered mutant, Cargo runs the observed target kinds and names across the workspace. For example, a library hit selects the workspace library tests. A hit in a doctest, a helper process, or a harness whose build artifact changes under the target filters uses the full suite. For literal edits, a passing clean library control lets Cargo try the same workspace library executables first. A library failure kills the mutant. A passing, timed-out, or unavailable library check proceeds to the full selected suite. To check results on nondeterministic tests, repeat the same shard with `--test-workspace` alone.

The native Linux runner copies the current workspace and keeps incremental artifacts under Cargo's target directory. Tests within a bundle share the copy. Use `-j 1` for suites that need exclusive access to external resources.

### Compiler-backed catalog execution

On native x86-64 Linux, pass the path to rustc from a toolchain with matching `rustc-dev` and LLVM libraries. The private driver was built against `1.102.0-nightly (a267dc927)`. The CLI builds the driver on first use and caches it under the target directory.

```sh
cargo online mutate --mir-rustc "$MIR_RUSTC" --test-workspace --workspace-coverage
```

The compiler matches primitive comparisons, bitwise operators and scalar function-body replacements by source span and operand or return type. One executable can contain both MIR and source variants. The native selector runs inside the function, including calls through function pointers. The driver checks the mapping in each compilation that loads the file. Ambiguous sites go through source dispatch or a literal build. A separately compiled workspace copy of the same native site requires a literal edit.

When the invoking Cargo supports the version-1 unit graph, the runner checks its package, target and feature sets against the clean compilation artifacts. It follows individual build-script and proc-macro dependencies in that graph. This lets a runtime feature of a library use a shared build even when a different build-time instance of its parent disables that feature. With stable Cargo the runner uses its conservative package-graph check. Set `CARGO` to a nightly Cargo executable to use unit graphs.

Both engines remember which candidates previously needed independent compilation. On a repeat they can send those edits straight to parallel literal workers. The cache holds build choices. Every candidate still receives a fresh classification.

### Comparison-only mode

The original runner flips runtime comparison results and uses the standard library unit-test harness. It can run independently of cargo-mutants:

```sh
cargo online mutate-comparisons -p my-library -j 4
cargo online mutate-comparisons -p my-library -j 4 --full
```

The default run executes each non-ignored library unit test once to check the baseline and record the comparisons it reaches. For each mutant, it runs the covering tests fastest first and stops at the first failure or timeout. Independent mutants run in parallel. `--full` tries every runnable unit test for each mutant, with the same early exit. Use it to check whether coverage-based selection missed a kill, particularly for nondeterministic tests.

The report gives each comparison's source location, outcome, and the test that killed it or timed out. **Uncovered** means no baseline test reached the comparison. **Survived** means all selected tests passed. The default per-process timeout is 60 seconds and can be set with `--timeout SECONDS`. The command exits with 0 when all mutants are killed, 1 when mutants survive, lack coverage, or time out, and 2 for a build, baseline, or runner error.

#### Comparison-only targets

Comparison-only mode supports native Linux libraries with the standard libtest harness and default Cargo features. It negates the boolean result of equality and ordering comparisons in runtime function bodies, preserving operand evaluation. It follows conventional library modules and skips explicit test functions and `#[cfg(test)]` modules, const functions, macro expansions, and `#[path]` modules. Other conditional compilation can leave inactive sites in the report. This mode runs library unit tests.

The command copies the workspace to a temporary session under Cargo's target directory, builds there, and keeps dependency artifacts under `target/cargo-online/build`. The workspace must be self-contained. Symlinks must resolve inside the workspace. The copy omits Git metadata, scratch directories, and build outputs. Test processes share the copied package directory and inherit the caller's environment. Use `-j 1` for tests that need exclusive access to shared resources.

### Rama workspace benchmark

At Rama revision `a8e7406af9d9483fb06506895b6639eb31107c8f`, workspace discovery matched all 67,884 cargo-mutants candidates across 55 packages. For an execution benchmark, we selected 115 candidates from four libraries and ran each against its owning library's unit tests.

| Runner | End-to-end seconds with a fresh target | Shared compilation |
|---|---:|---|
| cargo-online `eef2d17` | 276.184 | 0 candidates |
| cargo-online `8624d27` | 169.565, 169.945 | 69 candidates in 5 builds |
| cargo-mutants 27.1.0 | 186.092 | Individual edits |

Every run agreed by name on the 46 killed and 39 unviable mutants. Of the 30 that cargo-mutants missed, cargo-online reported six survivors and 24 uncovered by the selected unit tests. Runs used rustc 1.99.1 on x86-64 Linux, default features, four Cargo build jobs, and disabled debug info in the test and dev profiles. Dependencies were already downloaded. Reproduce the selection from the Rama workspace root:

```sh
cargo online mutate --lib -j3 --timeout 45 --build-timeout 600 -- \
  --file 'rama-core/src/**' --file 'rama-error/src/**' \
  --file 'rama-http-types/src/**' --file 'rama-utils/src/**' \
  --sharding round-robin --shard 0/64
```

#### Downstream workspace tests

A separate five-candidate shard used the full Rama workspace suite, including integration tests and doctests. The unfiltered run spent 1,128 s in test invocations and 289 s in builds. It killed two mutants, found two unviable and left one survivor. Workspace coverage identified that survivor as uncovered, killed the `rama-net` edit with library tests, and caught the `rama-utils` edit through full-suite fallback. Cargo-mutants confirmed the last kill in the `NonEmptyVec::binary_search_by_key` doctest.

| Runner | Elapsed seconds | Clean baseline |
|---|---:|---|
| cargo-online, full workspace | 1,420.673 | Whole workspace |
| cargo-online, workspace coverage | 995.725, 1,015.167 | Whole workspace |
| cargo-mutants 27.1.0 | 1,002.611 | Five mutated packages |

Runs used one mutation worker, 12 Cargo build jobs, an existing dependency cache, fresh source snapshots, default features, disabled dev/test debug info, and local socket access. Both completed coverage runs matched every named outcome after counting uncovered mutants as survivors. An additional attempt stopped in the unmodified baseline when a Rama CLI test encountered a loopback port collision. That test passed in isolation and the unchanged full retry completed. The sample shows a 28.5–29.9% saving against unfiltered cargo-online. The two coverage timings bracket the cargo-mutants result. Reproduce the two cargo-online modes from the Rama workspace root:

```sh
cargo online mutate --test-workspace -j1 --build-timeout 1200 -- --sharding round-robin --shard 0/16384
cargo online mutate --test-workspace --workspace-coverage -j1 --build-timeout 1200 -- --sharding round-robin --shard 0/16384
```

With the pinned MIR driver, a separate run used all workspace library tests and selected two edits individually. The suite killed the comparison in `rama-fastcgi::decode_length_from_slice` in one native MIR build (269.241 s). It also killed the depth-limit comparison in `rama-error` through an independent build (150.920 s). Some workspace build scripts depend on `rama-error`, so an edit there must also take effect while Cargo builds the workspace. These are two end-to-end samples from the 67,884 candidates.

On this revision, the pinned nightly enables six obsolete nightly-only benches in the `rama-http-headers` test harness. For both runs a compiler wrapper removed that crate's `--cfg nightly` only when compiling its test harness. Tests had access to local sockets. A repeat using nightly Cargo also stopped at the unmodified baseline because two gRPC example tests read `OUT_DIR` at runtime and that invocation omitted it.

#### Unfiltered workspace suite with the MIR driver

At `191f2ff`, the pinned driver ran the same five-candidate shard with `--test-workspace --full`. The clean workspace suite took 227.6 s and reported 9,261 passing tests before doctests and 652 passing doctests. The run took 1,432.185 s end to end. Cargo spent 270.91 s in explicit builds and 1,157.60 s in test invocations. Other benchmark processes overlapped part of the run.

The outcomes matched the earlier unfiltered run by name. The fuzz-input edit survived, the two return replacements in `rama-core` and `rama-http-core` were unviable, and the `rama-net` path-matcher edit failed a library test. A `NonEmptyVec::binary_search_by_key` doctest killed the `rama-utils` edit. The runner used one shared source build and four individual builds. This shard exercises full-suite compatibility and contains no native MIR variants.

A disjoint four-candidate shard (`8192/16384`, round-robin) took 1,120.460 s. It found two kills and two unviable edits, all from individual builds. A downstream gRPC example caught the `rama-http-types::decode_int` HPACK comparison. Together, these two shards establish nine outcomes from the 67,884-member catalog.

The benchmark copy enabled the nightly test crate and provided the six header benches with the current `TypedHeader` trait under their expected name. It also used the pinned nightly's diagnostic text for the existing `case-warning.rs` compile-fail test. All six benches and the diagnostic test passed. The original and configured workspace produced byte-identical lists of all 67,884 candidates.

### Rustyguard benchmark

#### Full cargo-mutants catalog

On Rustyguard revision `a8a0c62fd639da0e5103989023597539d39ce6d6`, cargo-online and cargo-mutants 27.1.0 agreed on every candidate by name and outcome. Both used the default Cargo test suite, default features, three workers, and a 15-second test timeout on x86-64 Linux with rustc 1.99.1.

| Package | Candidates | Killed | Survived | Unviable | Timed out | Shared compilation |
|---|---:|---:|---:|---:|---:|---|
| rustyguard-core | 249 | 65 | 64 | 119 | 1 | 129 candidates in 8 runnable bundles |
| rustyguard-types | 13 | 4 | 5 | 4 | 0 | 9 candidates in 1 runnable bundle |

One cold-target trial on `rustyguard-core` took **45.122 s** with cargo-online and **48.247 s** with cargo-mutants, with dependencies already downloaded. Cargo-online compiled 120 source edits individually, including all 119 unviable candidates. Both tools waited for the same 15-second timeout. These timings include discovery, building, and testing. The shared-build count includes successful bundles and excludes failed build attempts.

```sh
cargo online mutate -p rustyguard-core -j 3 --timeout 15
cargo mutants -p rustyguard-core -j 3 --timeout 15
```

#### Throughput with the pinned MIR driver

On the same 249 `rustyguard-core` edits, both modes agreed by name and classification with cargo-mutants 27.1.0. Runs used the owning package's normal Cargo suite, the same pinned rustc, three mutation workers, six Cargo jobs, a 15-second timeout and warm dependency builds.

| Engine | Without learned routes (s) | Repeat 1 (s) | Repeat 2 (s) |
|---|---:|---:|---:|
| Source bundles | 47.236 | 32.631 | 33.252 |
| MIR and source bundles | 56.383 | 34.006 | 33.530 |

The source run without learned routes was measured separately. The two repeat pairs alternated on an otherwise idle benchmark host. On repeat runs both engines shared 129 edits across five builds and compiled 120 individually. The driver handled 24 edits directly in MIR. Previously learned build choices reduced work on repeat runs. All runs executed the tests again. These timings do not establish a throughput advantage for the MIR driver over source bundles. Cargo-mutants reported 45 s in its independent run.

To reproduce, run `cargo online mutate -p rustyguard-core -j3 --timeout 15` with `RUSTC` and `RUSTDOC` pointing to the pinned toolchain and debug info disabled in the dev and test profiles. Add `--mir-rustc "$RUSTC"` for the MIR run.

#### Comparison-only benchmark

On the Rustyguard workspace at `a8a0c62fd639da0e5103989023597539d39ce6d6`, both modes killed the same individual mutants in all three trials. Timings are medians on x86-64 Linux with rustc 1.99.1, a release build of cargo-online, warm dependency artifacts, default test profiles, four workers, and a 45-second per-test timeout. Total time includes the workspace copy, instrumented build, baseline tests, and mutant runs.

| Package | Mode | Mutants | Baseline tests | Mutant test executions | Killed / survived / uncovered | Mutation time | Total time |
|---|---|---:|---:|---:|---:|---:|---:|
| rustyguard-core | Selected | 31 | 18 | 56 | 21 / 3 / 7 | 0.110 s | 0.874 s |
| rustyguard-core | Full | 31 | 18 | 324 | 21 / 10 / 0 | 0.384 s | 1.195 s |
| rustyguard-utils | Selected | 6 | 9 | 8 | 5 / 1 / 0 | 10.170 s | 10.531 s |
| rustyguard-utils | Full | 6 | 9 | 22 | 5 / 1 / 0 | 10.235 s | 10.551 s |

A separate control on `rustyguard-core` compiled each of the 31 negations individually and ran the full library unit suite with four test threads per invocation. After warming its baseline build, that single trial took 9.899 seconds and agreed on all 21 kills. The bundled run combines build reuse, test selection, and early exit, so this comparison measures their combined effect. On `rustyguard-utils`, selection reduced test executions with little effect on elapsed time.

To repeat the bundled measurements from the Rustyguard workspace, run the following pair three times for each package after warming the dependency cache:

```sh
cargo online mutate-comparisons -p rustyguard-core -j 4 --timeout 45
cargo online mutate-comparisons -p rustyguard-core -j 4 --timeout 45 --full
```

## Edit-to-test feedback

```sh
cargo online dev --mir-rustc "$MIR_RUSTC" --workspace --watch
cargo online dev --mir-rustc "$MIR_RUSTC" -p my-library --lib --nextest
cargo online dev --mir-rustc "$MIR_RUSTC" -p my-library --lib --fast
```

The driver instruments the selected library test harnesses, fingerprints function bodies using source and optimized MIR, and records which functions ran on each named test thread. A virtual workspace selects all member libraries. Use `--workspace` at a package root to track all member libraries, or `-p` to focus on one. The next run tries previously failing tests and tests observed in changed functions first. A failure is reported immediately. After a passing priority run, the default validates the workspace, including integration tests and doctests. `--lib` limits validation to library unit tests. `--full` starts with the full chosen suite. With `--lib --nextest`, cargo-nextest runs the library tests in separate processes.

`--fast` is an opt-in heuristic for one library. A partial pass requires a previously observed test for every changed body, successful execution of all selected tests and an unchanged workspace fingerprint outside ordinary top-level function bodies in that library's root source file. Changes to modules in other files, test declarations, manifests or unobserved functions take the full library path. A partial pass retains the coverage history of the last full run. External data, threads without a test identity and changes to execution paths can affect observed coverage.

`--watch` cancels the superseded process group on an edit and reuses Cargo's incremental target. The CLI reports time to the first test result, first failure and completion. Run `experiments/edit-latency.sh "$MIR_RUSTC"` from this checkout after a release build to reproduce the controlled edit benchmark.

### Incremental function objects

Add `--instance-codegen` to a development run to try finer-grained object reuse. The driver consults rustc's monomorphised item graph and splits eligible local functions into individually named codegen units. Items with explicit linkage, local inline copies, and functions that need a private symbol in their original unit keep their original placement. Rustc tracks dependencies, reuses valid object files and relinks the test executable. This experimental mode uses a separate incremental target. More objects can increase codegen and link time.

The integration test changes one of two functions, checks that the edited object's inode changed and that rustc reused the other object's inode, and observes the intended failing test. On Rama revision `a8e7406af`, the driver isolated 15 instances in 59 codegen units for `rama-error`. All 45 library tests passed.

### Edit-to-first-failure benchmark

Run `experiments/edit-latency.sh "$MIR_RUSTC" /path/to/rama` after a release build. The script extracts the pinned `rama-error` sources, warms each mode on a passing revision, inverts the depth-limit comparison, and checks that the expected truncation test fails. It uses one test thread. The full library has 45 tests and the priority set has 28. These elapsed seconds include compilation, linking and execution up to the first failure.

| Mode | Trial 1 | Trial 2 | Trial 3 |
|---|---:|---:|---:|
| Full library | 0.336 | 0.358 | 0.386 |
| Prioritised library tests | 0.345 | 0.388 | 0.380 |
| Prioritised, per-function codegen | 0.270 | 0.310 | 0.330 |

Without the Rama path the script uses a two-test fixture with a 300 ms unrelated test. Full validation reported the failure in 0.451, 0.462 and 0.478 s. Prioritisation reported it in 0.166, 0.169 and 0.153 s. On the real library, prioritisation alone had a slightly higher median than full validation. The per-function result is specific to this crate, edit and host.

## Longer-term design

The long-term goal is an incremental Rust development compiler and test engine: rebuild only what changed, automatically run impacted tests, and share compiler analysis and machine code across mutation variants. The sections below describe the proposed compiler and watch architecture. The mutation engine supports both source dispatch and an optional Cargo workspace compiler wrapper.

## The developer experience

Start a watch session:

```sh
cargo online dev --mir-rustc "$MIR_RUSTC" --workspace --watch
```

On every save, the tool should:

1. Detect and coalesce file changes, assigning them a source revision.
2. Incrementally reanalyse the affected Rust items and identify invalidated code-generation artifacts.
3. Recompile the minimum required code, then relink a **consistent executable snapshot**.
4. Select the tests potentially affected by those changes, using compiler dependencies and previously observed execution.
5. Run those tests automatically and in parallel, ideally through Nextest.
6. Display failures immediately, prioritise previously failing tests on the next edit, and refresh the test-impact index.

The goal is a Bacon-like background workflow with finer-grained rebuilding and automatic *impacted-test* selection instead of blindly repeating all tests.

For exhaustive validation, explicitly ask for the full suite; the selective mode must be honest about incomplete dependency evidence.

```sh
cargo online test --all           # proposed
cargo online mutate               # implemented for the cargo-mutants catalog
cargo online mutate --changed     # proposed: mutation analysis of changed code
```

## Two modes, one engine

| | Development mode | Mutation mode |
|---|---|---|
| Objective | Minimise time to useful feedback | Maximise mutants classified per second |
| Trigger | File edits / watch session | Explicit or background job |
| Work selection | Invalidated compilation artifacts + potentially impacted tests | MIR mutation sites + relevant tests |
| Codegen | Prioritise critical path to runnable snapshot | Batch independent mutated function variants |
| Linking | As soon as a coherent test executable is ready | Amortise across mutation bundles |
| Test order | Previous failures and quick tests first | Likely-to-kill / runtime ratio |
| Cancellation | Supersede work for stale edits | Stop scheduling tests once mutant is killed |
| Parallelism | Low latency and responsiveness | High sustained CPU/memory utilisation |

A third *opportunistic* policy could run low-priority mutation analysis or refresh stale tests while the developer is idle, yielding promptly to new edits.

## Proposed architecture

```text
                    Cargo workspace + filesystem watcher
                                   |
                       revision-aware coordinator
                                   |
           +-----------------------+---------------------+
           |                       |                     |
     rustc frontend          dependency/index       test discovery
   (HIR, types, MIR)         + artifact cache         + coverage
           |                       |                     |
           +------ affected monomorphised instances ----+
                                   |
                     MIR optimisation / mutation
                                   |
                       parallel function codegen
                                   |
                        object / symbol cache
                                   |
                         linker (e.g. Wild)
                                   |
                      coherent test executable(s)
                                   |
                         Nextest / test runner
                                   |
                      result + coverage updates
```

The **coordinator** should own global task priorities, revision consistency, resource budgets, and cancellation. Compiler workers should not need to know whether they are serving dev or mutation mode. The same cached artifacts and dependency indexes feed both.

### Three indexes, not a giant global call graph

Avoid materialising the transitive closure of all possible test-to-function dependencies. Keep three complementary *sparse* structures and join them only when needed:

1. **Compiler dependencies:** Direct, typed edges between items, MIR/codegen queries, inlined bodies, compile-time constants, type layouts, etc. These determine what actually needs recompiling.
2. **Link dependencies:** Symbol definitions and relocations from code/data objects to other symbols. These determine what can be repaired by relinking, without recompilation.
3. **Test execution index:** Per-test observed coverage of functions or MIR blocks. Store a reverse index from executed region to compressed sets of test IDs (e.g. Roaring bitmaps), rather than explicit paths through the call graph.

Example:

```text
const TIMEOUT --compile dependency--> fn connect
fn connect   --observed execution----> {test_timeout, test_retries}
fn helper    --relocation------------> symbol handler
```

For a change to `TIMEOUT`, the compiler determines the changed code/data artifacts; the test index then finds tests that previously executed those artifacts. The linker handles address changes through relocations.

The expected storage is proportional to the **direct compiler edges + linker relocations + observed coverage relations**, not the number of all possible call paths. Avoid materialising per-mutant copies of the graph.

### Function pointers, trait objects, constants, and statics

Because executables are **rebuilt/relinked**, not hot-patched, a changed address-taken function can normally be recompiled and its references relocated by the linker. It is not necessary to statically enumerate every possible caller through a function pointer. Test execution instrumentation observes which target actually ran.

- **Function pointers and dynamic dispatch:** Trace executed targets during tests. Preserve conservative handling when pointers escape, are compared without being invoked, or may affect routing following a change.
- **Inlining:** If a function body was inlined into a caller, that caller has a *codegen* dependency and may need recompilation. Ordinary non-inlined calls can often be handled through linker relocations.
- **`const` and compile-time evaluation:** Values may be embedded into code; track the resulting compiler/codegen dependencies, then select tests of all invalidated artifacts.
- **`static`:** Track initialization, references, and compile-time assumptions. Existing data references can be relocated when producing a fresh binary; an initializer or layout change may invalidate more code/data.
- **Vtables, drop glue, generics, and type layouts:** Track their generated artifacts and ABI dependencies. A signature, layout, trait-resolution, or monomorphisation change may require broad invalidation.
- **FFI, unsafe escapes, macros, build scripts, environment or filesystem inputs:** Be conservative if analysis cannot prove the boundary is contained.

Do **not** attempt to keep static addresses or state stable across an already-running test process. No live code patching is required; fresh test processes naturally initialise their own state.

### Sound test selection: an explicit trust model

**Observed coverage is not a proof of all future dependencies.** A source change can modify which branches or callees a test reaches, and code can observe data without entering the changed function. The system should distinguish:

- **Proven unaffected:** Safe to reuse or skip, under stated compiler and runtime assumptions.
- **Potentially affected:** Run the test.
- **Unknown / opaque:** Broaden selection, potentially up to the full suite.

Proposed policies:

| Policy | Behaviour |
|---|---|
| `fast` | Use observed coverage aggressively; best latency, possible misses |
| `safe` (default) | Combine coverage with conservative compiler/dependency invalidation and fallbacks; does not promise perfect soundness |
| `full` | Run all discovered tests, refreshing coverage and validating the selection model |

New/changed tests always run. Major structural edits (e.g. test registration, feature changes, macro expansion, build-script behaviour) can invalidate indexes and trigger broader runs. Track coverage and results by **revision, compiler settings, feature set, target, and test identity**; never accidentally apply a result from the wrong executable snapshot.

For mutation testing, test selection can be finer: restrict to tests that reached a mutated MIR block where justified, otherwise fall back to function-level or broader coverage. Validate the strategy experimentally by comparing selected runs against full-suite executions and recording false negatives.

## MIR as the shared analysis boundary

MIR sits after the Rust frontend's parsing, macro expansion, name resolution and type checking, and before final machine code generation. Rustc also uses MIR for borrow checking.

For common behaviour-preserving-type mutations (e.g. operator replacements, comparison inversion, branch changes), generate variants from a **validated MIR stage** and avoid repeating parsing, macro expansion, type checking, and borrow checking for each variant *where the transformation provably preserves the required invariants*. Arbitrary MIR modifications are not automatically sound.

```text
frontend once → validated MIR
                       |
                 mutation generator
                  /    |    \
                M1    M2    M3
                  \    |    /
                  codegen variants → shared test binary / bundles
```

The cache's true codegen identity is closer to **(definition, concrete generic arguments, compiler/target context)** than to a source-level function name. Keys also need dependencies on MIR, inlined bodies, layouts, codegen settings, relevant constants, and compiler/toolchain version.

Rustc already has fine-grained incremental queries, but its normal codegen reuse and object generation work largely at CGU granularity. A useful experiment is per-monomorphised-instance compilation, while measuring object-file count, backend startup, cache overhead, cross-function optimisation loss, and linker cost. One object per function is a prototype strategy, **not necessarily the final storage format**.

## Mutation-testing execution

The important optimisation is sharing the expensive baseline work across many mutants:

1. Produce candidate MIR mutations and associate each with a function/block identity.
2. Generate a modest batch of variants from already-validated MIR.
3. Compile variants independently (or as a bundle) and reuse unchanged compiled artifacts.
4. Link one executable per bundle, or optionally use a startup-selected dispatcher to choose the active variant.
5. Run selected test × mutant pairs in separate processes.
6. As soon as a test kills a mutant, stop scheduling further tests for that mutant.

Bundling must preserve function-pointer and vtable behaviour, which may require variant-aware dispatch. Some mutations (const/static initialisers, layout/type changes) need separate executable variants or broader rebuilding. The initial implementation can simply relink each mutant and optimise batching later.

### Nextest integration

Use [cargo-nextest](https://nexte.st/) for normal test discovery and parallel process-isolated test execution, rather than writing a test runner from scratch.

Two possible mutation-mode integrations:

- **Baseline:** Build/link a mutant test executable, then invoke Nextest with the relevant test filter(s). Simple and immediately measurable, but repeated orchestration can cost time.
- **Experiment:** A custom libtest-compatible harness exposes selected `mutant × test` pairs as virtual tests, so Nextest schedules them as separate processes. This reuses its concurrency, timeouts and reporting. Dynamic mutant-aware cancellation still requires careful coordination or a specialised scheduler.

The coordinator should avoid eagerly scheduling dozens of tests for the same mutant. Initially prefer one or two in-flight tests per mutant and replenish work as tests complete. Reuse historical runtime and kill effectiveness to prioritise candidate tests.

Nextest remains the preferred **execution backend** for development mode; deeper integration is optional and should be benchmark-driven.

## Parallelism and scheduling

Represent work as a DAG of analysis, MIR transform, codegen, link, and test tasks. Each task carries a revision, dependencies, estimated CPU/memory cost, and priority.

- **Frontend:** Exploit rustc's existing parallel query processing where possible; avoid inventing a second parallel compiler frontend prematurely.
- **Codegen:** Run independent function instances or mutation variants concurrently; enforce memory limits.
- **Linking:** Relink coherent snapshots, possibly in parallel across independent binaries; avoid excessive link fan-out.
- **Tests:** Run separate test processes concurrently with Nextest; respect resource-heavy tests and local machine limits.
- **Cross-stage scheduling:** Permit pipeline overlap when correct, but prioritise the critical path in dev mode and sustained classification throughput in mutation mode.

Development-mode tasks from an older revision may be cancelled or deprioritised, but keep reusable intermediate cache entries. Test results belong to their exact revision, never an implicitly mutable current build.

## Build strategy: avoid a permanent rustc fork initially

Suggested progression:

| Phase | Deliverable | Likely integration |
|---|---|---|
| 0 | Establish baseline timings and correctness corpus | Stable Cargo/Nextest, nightly profiling |
| 1 | Per-test coverage index + automatic impact selection/watch UI | Cargo wrapper + Nextest + coverage instrumentation |
| 2 | Extract MIR and classify dependencies/mutation sites | Nightly `rustc_private` custom `rustc_driver` |
| 3 | MIR mutation prototype; reuse frontend within a compilation | Custom driver; targeted rustc patches if necessary |
| 4 | Fine-grained codegen artifact cache and incremental relinking | Backend/CGU experiments; likely rustc changes + Wild |
| 5 | Long-lived workspace frontend/query state | Substantial rustc architecture work, upstream collaboration |

Early versions should allow normal rustc processes to exit, with an external daemon maintaining artifact and coverage caches. Keeping a mutable `TyCtxt` alive across arbitrary edits entails much harder problems around compiler lifetimes, query invalidation, stable identities, deleted definitions, and memory reclamation.

Use a **small experimental rustc patch or fork only at a concrete API boundary**; prefer upstreamable changes over a long-running divergent compiler. Follow upstream work on fast builds and Relink-Don't-Rebuild rather than duplicating it.

## Measuring whether it works

Compare against `cargo test`, `cargo nextest run`, and a Bacon-style watch workflow on representative Rust workspaces. Record:

- Cold build vs no-op vs one-function edit vs API/layout change.
- Wall-clock to first relevant test result; time to all selected tests; time to full validation.
- Frontend time, MIR time, codegen/cache hits, link time, test time.
- Cache size, memory peak, generated object count, contention under parallel builds.
- Mutation throughput (mutants conclusively classified per second), work avoided after first kill, and false-negative rate of selected test sets compared with full-suite runs.
- Correctness over function pointers, statics, `const`, generics, trait objects, inlining, build scripts, proc macros, FFI and changing feature sets.

A worthwhile early prototype is *just* automatic selected-test execution with an independently refreshable coverage index. It delivers value without requiring a persistent Rust compiler.

## Non-goals (initially)

- Hot-patching machine code in a running process or retaining live static state.
- Guaranteed precise test selection for arbitrary program behaviour, external I/O or unsafe/FFI code.
- Reimplementing Cargo package management, Nextest scheduling, or Rust's type checker.
- Optimised release builds as the first target. Start with development/test profiles.
- Replacing CI's full validation with selective tests.

## Open design questions

- How far can a `rustc_driver` and query overrides go before changes to rustc are unavoidable?
- Which MIR stage is best for mutations and codegen caching while preserving validation invariants?
- Can fine-grained codegen reuse beat normal CGU caching after accounting for object and link overhead?
- How should monomorphised instances be mapped to source definitions for coverage without exploding indexes?
- How do we represent conservative static-address escapes, dynamic dispatch rerouting and external data dependencies?
- Can mutant variants be bundled without unacceptable dispatch or code-size overhead?
- Can the daemon share cached artifacts across feature profiles and independent workspace invocations safely?
- What level of Nextest integration is required for mutation-aware early termination?
- What is the correct balance between low-latency edit feedback and background validation?

## Related projects

- [Rust compiler development guide](https://rustc-dev-guide.rust-lang.org/) — MIR, queries, incremental compilation and codegen internals.
- [Rust compiler performance tooling (`measureme`)](https://github.com/rust-lang/measureme) — profiling rustc phases and queries.
- [Rust 2026 Fast Builds roadmap](https://goals.rust-lang.org/2026/roadmap-fast-builds.html) — relevant upstream compiler and linker work.
- [Wild](https://github.com/davidlattimore/wild) — fast linker and candidate for future incremental link integration.
- [cargo-nextest](https://nexte.st/) — test discovery, execution, resource control and reporting.
- [Bacon](https://github.com/Canop/bacon) — background file-watch development experience.
- [cargo-affected](https://github.com/max-sixty/cargo-affected) — experiment with per-test execution coverage and changed-code test selection.
- [rustc_codegen_cranelift](https://github.com/rust-lang/rustc_codegen_cranelift) — alternative codegen backend and useful comparison for fast development builds.

---

**Status:** The catalog runner supports mixed MIR and source bundles on the pinned compiler, with independent Cargo builds for remaining edits. Development mode supports coverage-based prioritisation, watch cancellation and experimental per-instance object reuse. Incremental linking and a frontend that retains query state in memory across edits remain open. The [standalone equality experiment](experiments/mir-equality.rs) demonstrates the original single-site query override.
