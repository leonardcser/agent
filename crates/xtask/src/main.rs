//! Dev tooling. Invoke as `cargo xtask <command>` (see `.cargo/config.toml`).

#[global_allocator]
static ALLOCATOR: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

mod bench_file_search;
mod bench_lineage_search;
mod bench_store_compression;
mod bench_support;
mod bench_transcript_layout;
mod fuzz;
mod gen_file_icons;
mod gen_lua_docs;
mod prepare_release;
mod synth;

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("xtask is at crates/xtask/")
        .to_path_buf()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("bench-diff") => {
            for (package, test_filter, features) in [
                ("smelt-core", "diff_index_benchmark", &[][..]),
                ("smelt-tui", "diff_viewer_benchmark", &["harness"][..]),
                ("smelt-tui", "diff_loading_benchmark", &["harness"][..]),
            ] {
                bench_support::run_cargo_test_benchmark(bench_support::CargoTestBenchmark {
                    package,
                    test_filter,
                    features,
                    release: true,
                    env: Vec::new(),
                    bench_name: test_filter,
                    ignored: true,
                });
            }
        }
        Some("bench-file-search") => bench_file_search::run(args.collect()),
        Some("bench-lineage-search") => bench_lineage_search::run(args.collect()),
        Some("bench-store-compression") => bench_store_compression::run(args.collect()),
        Some("bench-transcript-layout") => bench_transcript_layout::run(args.collect()),
        Some("gen-file-icons") => gen_file_icons::run(args.collect()),
        Some("gen-lua-docs") => gen_lua_docs::run(),
        Some("prepare-release") => prepare_release::run(args.collect()),
        Some("synth") => synth::run(),
        Some("fuzz") => fuzz::run(args.collect()),
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`");
            print_usage();
            std::process::exit(2);
        }
        None => {
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask <command> [args]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  bench-diff                            benchmark indexed diffs and million-line overlay frames");
    eprintln!(
        "  bench-file-search [--runs N] [--entries N] [--queries CSV] benchmark file fuzzy search"
    );
    eprintln!(
        "  bench-lineage-search --state-dir PATH [--session ID] [--runs N] [--allow-debug] benchmark production derived search"
    );
    eprintln!(
        "  bench-store-compression STATE_DIR [--max-samples N] [--max-bytes N] sample store compression"
    );
    eprintln!(
        "  bench-transcript-layout [--runs N] [--workloads CSV] [--scale-500mb] run transcript layout benches"
    );
    eprintln!("  gen-file-icons [DEVICONS_DIR]          regenerate nvim-web-devicons registry");
    eprintln!("  gen-lua-docs                          regenerate Lua API stubs + reference docs");
    eprintln!("  prepare-release VERSION               materialize a tag version in this checkout");
    eprintln!(
        "  synth                                 generate a synthetic session for perf testing"
    );
    eprintln!("  fuzz run <target> [--fork N] [--cmin] fuzz a target until crash or Ctrl-C");
    eprintln!(
        "  fuzz triage <target> <artifact>       shrink a crash and print the minimal scenario"
    );
    eprintln!("  fuzz replay-regression                replay every committed regression seed");
    eprintln!("  fuzz coverage-snapshot [target...]    per-target source coverage snapshot");
}
