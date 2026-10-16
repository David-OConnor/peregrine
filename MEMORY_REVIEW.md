**Peregrine memory review — October 16, 2026**

**Implementation update:** The search allocation and terminal fixes below are now implemented. The original review and its before measurements follow this update.

- Search collects only visible match ranges. Each of four workers has an 8 MiB line-buffer limit; incomplete files are reported in the results panel.
- Terminal output is parsed once into bounded styled lines. Rendering retains exact wrapped heights and draws visible lines; selections copy from the model, including offscreen text. The per-tab content/style budget is 4 MiB with a 20,000-line cap and a 16 KiB cap on individual lines. Closed tabs share an 8 MiB allocation budget, retaining their navigation history when older output is released.
- Command output uses a 64-chunk bounded queue with backpressure and bounded UI polling. Stable line IDs preserve stream continuation and stderr recoloring when old output is evicted. Clear releases scrollback allocations.
- The 10,000-line, 1,010,000-byte release probe now retains **7.0 MiB of Rust heap**, versus **188.8 MiB** before. The first populated frame took **2.6 ms**, and the third took **0.14 ms**, versus **124 ms** and **9.8 ms** before. All 10,000 lines were retained in this comparison. After Clear and cache expiry, heap returned to **2.3 MiB**. These remain headless egui measurements, excluding graphics overhead.
- All 19 release regression tests passed, including real command backpressure/Stop, UTF-8 and ANSI boundaries, error coloring, wrapping, offscreen selection copying, scroll-position preservation during eviction, shared closed-tab retention, and oversized search lines. The graphics backend is unchanged.

Post-fix profiling source and logs are in `target/memory-after`. A separate uninstrumented release executable was successfully built at `target/release/peregrine-memory-fixed.exe`, using a source copy with the same dependencies, features, and version. The standard executable could not be replaced because Windows denied access.

---

There are worthwhile optimizations. The roughly 250 MB idle footprint is largely explained by the current graphics configuration on this machine. Separately, terminal rendering and deep search have much larger memory costs than their input sizes suggest. Those are the most useful application-level targets.

I reviewed the current working source, including the local `shell` dependency and the installed dependency implementations. I built an instrumented release copy under `target/memory-review`; the application source and existing release executable were left unchanged. The probe redirects state writes to its own state file. It never runs user commands or changes the system clipboard.

All sizes below are MiB (1,048,576 bytes). These are measurements from this Windows machine with an NVIDIA RTX 4080, not portable memory guarantees. Native window probes used a hidden 1400 × 900 window, requested frames every 100 ms, and sampled after four seconds. Ordinary use, visible windows, different drivers, and additional viewports can change the results.

| Native release probe | Windows working set | Private committed memory | Live Rust heap |
| --- | ---: | ---: | ---: |
| Almost empty window, default Vulkan renderer | 258.0 | 450.8 | 9.8 |
| Actual application UI, default Vulkan renderer | 269.9 | 460.9 | 19.1 |
| Same UI, DirectX 12 | 235.9 | 380.6 | 18.9 |
| Same UI, OpenGL / Glow | 158.5 | 162.7 | 18.7 |

“Working set” includes resident shared pages. “Private committed memory” includes private allocations that need not currently be resident. “Live Rust heap” counts requested allocations through Rust's global allocator; it excludes allocator overhead, native library allocations, mapped code, and graphics driver allocations. None is necessarily identical to the particular Memory column observed in Task Manager, and these columns should not be added together.

The initial saved state contained 627 command history entries, three tabs, 20 files in the active listing, and seven clipboard entries totaling 698 text bytes. Loading it used approximately **0.13 MiB of Rust heap**. That persisted data does not explain the observed idle footprint.

**1. Virtualize terminal output and bound retained scrollback.**

[Terminal rendering](C:/Users/the_a/code/peregrine/src/ui/mod.rs:636) walks every output item on every frame. For each item it parses ANSI escapes into owned strings, builds a layout job, and submits a label. Scrolling off screen does not prevent those operations or the font layout cache from retaining their results.

In a headless egui release probe, 10,000 distinct, single-line messages containing **1,010,000 bytes of text** produced these results:

| Terminal probe | Live Rust heap after warmup | First populated frame | Warm frame |
| --- | ---: | ---: | ---: |
| Current implementation | 188.8 MiB | 124 ms | 9.8 ms |
| Prototype drawing only visible rows | 5.2 MiB | 2.9 ms | 0.05 ms |

The current implementation requested about 384 MiB of cumulative allocations during its first populated frame and 11 MiB during the third frame. These are allocation traffic, not simultaneous live memory. The renderer and GPU were absent from this experiment, so its heap figures are separate from the native-window table.

Use visible-row rendering with a small overscan region and a bounded cache for parsed/layout data. The prototype uses fixed-height `show_rows` and demonstrates the opportunity; it is **not a drop-in implementation** for the existing UI. Production work must support wrapped and multiline items, preserve selection and copying, invalidate heights on width/font changes, and keep the tail pinned correctly.

[Stored output](C:/Users/the_a/code/peregrine/src/state.rs:1250) is also unbounded, independently of rendering. Apply both a byte budget and a line/item budget per tab, including output appended by `RunningCmd`. A byte limit matters because an individual item may contain many lines or a very long unfinished line. Eviction must adjust the running command's output indices and stderr recoloring boundaries.

[Closed tabs](C:/Users/the_a/code/peregrine/src/state.rs:1335) retain their full output for reopening. The existing limit of 20 tabs bounds their count, not their bytes. Add a shared byte budget for closed-tab snapshots, or compact old output while retaining directory/navigation state. Closing the synthetic output tab released its large layout cache after subsequent frames but intentionally retained the underlying text.

**2. Fix deep search's oversized match-range allocation.**

[`match_line`](C:/Users/the_a/code/peregrine/src/deep_search.rs:312) first collects every match in the complete input line, then clips the displayed text to 160 characters and collects only the overlapping ranges. In this release build, the second collection reused the first vector's allocation. Its logical length became small, but its capacity remained enormous.

Searching a **4 MiB line of `a` characters for `a`** returned 160 displayed matches plus an ellipsis, with a range-vector capacity of **4,194,304 entries**. That is **64 MiB retained for one result**, in addition to the input/search buffers. The 2,000-result limit does not prevent this.

An isolated prototype finds the first match to position the excerpt, then collects only ranges overlapping that excerpt and stops when later matches cannot overlap it. It keeps matching against the original line, preserving regex anchor context. The same test used a vector capacity of 256 entries, or **4 KiB**, instead of 64 MiB. Its text and ranges matched the existing implementation in 54 comparisons covering Unicode, anchors, empty matches, whitespace, and long lines.

Also give the [per-worker `SearcherBuilder`](C:/Users/the_a/code/peregrine/src/deep_search.rs:208) an explicit `heap_limit`. The installed grep-searcher defaults to no heap limit, and line-oriented searching must buffer a complete long line before the display truncation occurs. Account for the number of concurrent workers when choosing the limit, and report skipped oversized inputs rather than silently turning them into apparent nonmatches.

This is the smallest, most targeted fix in the review.

**3. Consider a lighter graphics backend for the idle footprint.**

[`NativeOptions`](C:/Users/the_a/code/peregrine/src/main.rs:254) uses eframe's defaults. With the current Cargo features, that selects wgpu; on this machine it selected Vulkan. The near-empty-window result shows that most of the idle footprint appears before substantial application data is present.

The DirectX 12 experiment used `WGPU_BACKEND=dx12` in the probe process's environment. It reduced working set by about **34 MiB** and private commit by about **80 MiB** relative to the default Vulkan run.

The Glow experiment enabled eframe's `glow` feature and explicitly selected `Renderer::Glow` in the isolated copy. It reduced working set by about **111 MiB** and private commit by about **298 MiB**. Rust heap stayed nearly the same, which supports treating this as graphics/native overhead rather than a large application collection.

OpenGL is therefore worth evaluating if idle RAM is a priority. Validate normal visible rendering, image/text/Markdown child windows, DPI changes, and supported Windows/Linux/macOS drivers before changing the shipped default. Enabling Glow also changes eframe's `on_exit` signature to accept the optional Glow context; the probe adapted its copied implementation accordingly. These are short local comparisons, not a cross-platform backend qualification.

**4. Bound command output waiting between the reader threads and UI.**

[`RunningCmd::start`](C:/Users/the_a/code/peregrine/src/running.rs:117) creates an unbounded channel. Its reader threads can enqueue output faster than the UI consumes it, particularly while the UI is blocked by another operation. [`poll`](C:/Users/the_a/code/peregrine/src/running.rs:165) drains until the queue is empty, so an aggressive producer can also monopolize a frame.

Use bounded output delivery with backpressure, plus a per-frame byte/time budget and another repaint when work remains. Coordinate this with the scrollback limit. Preserve stop, EOF, and process-exit handling when introducing backpressure. This is a code-confirmed growth path; no queue stress benchmark was needed to establish that the queue has no bound.

**5. Reduce image-decoding peaks and manage image-cache lifetime.**

[`decode_image`](C:/Users/the_a/code/peregrine/src/viewer.rs:455) decodes the original image before resizing it. The 2048-pixel preview limit bounds the final preview texture, not peak decoding memory. Resizing and converting RGB/RGBA buffers create additional allocations.

For a synthetic **6000 × 4000 RGB PNG**, decoding a Files-panel preview added approximately **202 MiB of peak Rust heap**, even though the completed preview pixels occupied only **10.7 MiB**. Decoding it at full size retained **91.6 MiB of CPU pixels before GPU upload**. Multiple viewer windows can decode independently; the existing one-job-at-a-time rule applies separately to each window and the preview.

Set explicit decode/pixel limits, use the displayed preview size instead of always allowing a 2048-pixel side, and consider a shared concurrency budget for image decoding. A 1024-pixel maximum side quarters the final preview pixel count relative to 2048, but does not by itself fix the full-resolution decode peak. Use scaled decoding where the chosen decoder supports it, and inspect avoidable intermediate pixel buffers. Keep enough resolution for zoomable full viewers as a separate policy.

Markdown images use egui's shared image loaders. The installed file/image loaders retain encoded bytes and decoded images by URI; the application has no corresponding `forget_image` calls when [viewer documents are removed](C:/Users/the_a/code/peregrine/src/viewer.rs:603). Add reference-aware eviction or a cache byte budget so images from closed documents can be released without evicting images still used by another open document. This cache-lifetime finding comes from source inspection, not a Markdown-image benchmark.

**6. Add a total clipboard-history byte budget.**

The [clipboard limits](C:/Users/the_a/code/peregrine/src/clipboard.rs:42) allow 100 entries of up to 1 MiB each. A synthetic maximum-size history used **100 MiB** of live heap. The background watcher's pending queue and current-content snapshot can temporarily hold additional copies.

Keep the count and per-entry limits, and add a shared total byte budget—for example, 8–16 MiB if that fits the intended clipboard workflow. Apply a byte budget to pending observations as well. The initial clipboard history was tiny, so this is a worst-case safeguard, not an explanation for today's idle memory.

**Lower-priority observations.**

Command history grows without a retention limit, and the history/file panels submit all their rows through `list_columns`; visible-row rendering would help very large histories or directories. At today's measured history size, the underlying data is small.

`clear_out` calls `Vec::clear`, which releases item strings but retains the vector's backing allocation. Replacing the vector or shrinking it on explicit Clear can reclaim that allocation. This is much smaller than the text layout cost; do not shrink hot-path buffers on every update.

Existing protections are useful: text reads are capped at 1 MiB of input, source highlighting is lazy and has line/section/time limits, image-navigation requests avoid piling up jobs within a window, and source highlighting shares a lazy syntax set. A fresh Markdown syntax/theme cache measured about 0.5 MiB, so combining syntax caches is a much lower priority than the items above. Removing dependencies or stripping the executable should not be assumed to produce comparable runtime-memory savings.

**Suggested implementation order:** fix deep-search range collection and search-buffer limits; virtualize terminal rendering with bounded scrollback and command delivery; evaluate the renderer default; then tighten image and clipboard budgets. This addresses both the idle footprint and the most substantial growth paths.

The profiling copy, counters, prototypes, and raw logs are in `target/memory-review` (Git-ignored). Rebuild it with `cargo build --manifest-path target/memory-review/Cargo.toml --release --offline --target-dir target`. The separate executable is `target/release/peregrine-memory-review.exe`; its modes are `native`, `blank`, `glow`, `virtual`, `search-baseline`, `search`, and `viewers`, with no argument selecting the original terminal test. `native` respects `WGPU_BACKEND`. These artifacts are disposable and are not part of the application build.
