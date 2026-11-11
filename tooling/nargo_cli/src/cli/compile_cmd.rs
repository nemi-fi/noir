use std::hash::BuildHasher;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use fm::FileManager;
use nargo::ops::{collect_errors, compile_contract, compile_program, report_errors};
use nargo::package::Package;
use nargo::workspace::Workspace;
use nargo::{insert_all_files_for_workspace_into_file_manager, parse_all};
use nargo_toml::PackageSelection;
use noir_artifact_cli::fs::artifact::{
    read_program_from_file, save_contract_to_file, save_program_to_file,
};
use noirc_artifacts::contract::CompiledContract;
use noirc_driver::NOIR_ARTIFACT_VERSION_STRING;
use noirc_driver::{CompilationResult, CompileOptions};

use clap::Args;
use noirc_frontend::hir::ParsedFiles;
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::{EventKind, RecursiveMode};

use crate::errors::CliError;

use super::{LockType, PackageOptions, WorkspaceCommand};
use rayon::prelude::*;

/// Compile the program and its secret execution trace into ACIR format
#[derive(Debug, Clone, Args)]
pub struct CompileCommand {
    #[clap(flatten)]
    pub(super) package_options: PackageOptions,

    #[clap(flatten)]
    pub(super) compile_options: CompileOptions,

    /// Watch workspace and recompile on changes.
    #[clap(long, hide = true)]
    watch: bool,
}

impl WorkspaceCommand for CompileCommand {
    fn package_selection(&self) -> PackageSelection {
        self.package_options.package_selection()
    }

    fn lock_type(&self) -> LockType {
        LockType::Exclusive
    }
}

pub(crate) fn run(args: CompileCommand, mut workspace: Workspace) -> Result<(), CliError> {
    // nemi patch: a worker process of a sharded compilation writes to its own target directory
    if let Some(target) = std::env::var_os(sharded::SHARD_TARGET_ENV) {
        workspace.target_dir = Some(target.into());
    }
    if !args.watch && !args.compile_options.debug_compile_stdin {
        if let Some(jobs) = sharded::jobs(&workspace) {
            return sharded::compile(&workspace, jobs);
        }
    }
    if args.watch {
        if args.compile_options.debug_compile_stdin {
            return Err(CliError::CantWatchStdin);
        }
        watch_workspace(&workspace, &args.compile_options)
            .map_err(|err| CliError::Generic(err.to_string()))?;
    } else {
        let debug_compile_stdin = None;
        compile_workspace_full(&workspace, &args.compile_options, debug_compile_stdin)?;
    }
    Ok(())
}

/// Continuously recompile the workspace on any Noir file change event.
fn watch_workspace(
    workspace: &Workspace,
    compile_options: &CompileOptions,
) -> notify_debouncer_full::notify::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();

    // No specific tickrate, max debounce time 1 seconds
    let mut debouncer = new_debouncer(Duration::from_secs(1), None, tx)?;

    // Add a path to be watched. All files and directories at that path and
    // below will be monitored for changes.
    debouncer.watch(&workspace.root_dir, RecursiveMode::Recursive)?;

    let mut screen = std::io::stdout();
    write!(screen, "{}", termion::cursor::Save).unwrap();
    screen.flush().unwrap();
    let debug_compile_stdin = None;
    let _ = compile_workspace_full(workspace, compile_options, debug_compile_stdin);
    for res in rx {
        let debounced_events = res.map_err(|mut err| err.remove(0))?;

        // We only want to trigger a rebuild if a noir source file has been modified.
        let noir_files_modified = debounced_events.iter().any(|event| {
            let mut event_paths = event.event.paths.iter();
            let event_affects_noir_file =
                event_paths.any(|path| path.extension().is_some_and(|ext| ext == "nr"));

            let is_relevant_event_kind = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            );

            is_relevant_event_kind && event_affects_noir_file
        });

        if noir_files_modified {
            write!(screen, "{}{}", termion::cursor::Restore, termion::clear::AfterCursor).unwrap();
            screen.flush().unwrap();
            let debug_compile_stdin = None;
            let _ = compile_workspace_full(workspace, compile_options, debug_compile_stdin);
        }
    }

    screen.flush().unwrap();

    Ok(())
}

/// Parse all files in the workspace.
pub fn parse_workspace(
    workspace: &Workspace,
    debug_compile_stdin: Option<String>,
) -> (FileManager, ParsedFiles) {
    let mut file_manager = workspace.new_file_manager();

    if let Some(main_nr) = debug_compile_stdin {
        file_manager.add_file_with_source(Path::new("src/main.nr"), main_nr);
    } else {
        insert_all_files_for_workspace_into_file_manager(workspace, &mut file_manager);
    }

    let parsed_files = parse_all(&file_manager);
    (file_manager, parsed_files)
}

/// Parse and compile the entire workspace, then report errors.
/// This is the main entry point used by all other commands that need compilation.
pub fn compile_workspace_full(
    workspace: &Workspace,
    compile_options: &CompileOptions,
    debug_compile_stdin: Option<String>, // use this String as STDIN if present
) -> Result<(), CliError> {
    let mut debug_compile_stdin = debug_compile_stdin;
    if compile_options.debug_compile_stdin && debug_compile_stdin.is_none() {
        let mut main_nr = String::new();
        let stdin = std::io::stdin();
        let mut stdin_handle = stdin.lock();
        stdin_handle.read_to_string(&mut main_nr).expect("reading from stdin to succeed");
        debug_compile_stdin = Some(main_nr);
    }
    let (workspace_file_manager, parsed_files) = parse_workspace(workspace, debug_compile_stdin);

    let compiled_workspace =
        compile_workspace(&workspace_file_manager, &parsed_files, workspace, compile_options)?;

    report_errors(
        compiled_workspace,
        &workspace_file_manager,
        &parsed_files,
        compile_options.deny_warnings,
        compile_options.silence_warnings,
    )?;

    Ok(())
}

/// Compile binary and contract packages.
/// Returns the merged warnings or errors.
fn compile_workspace(
    file_manager: &FileManager,
    parsed_files: &ParsedFiles,
    workspace: &Workspace,
    compile_options: &CompileOptions,
) -> Result<CompilationResult<()>, CliError> {
    let (binary_packages, contract_packages): (Vec<_>, Vec<_>) = workspace
        .into_iter()
        .filter(|package| !package.is_library())
        .cloned()
        .partition(|package| package.is_binary());

    // Compile all of the packages in parallel.
    let program_warnings_or_errors: CompilationResult<()> =
        compile_programs(file_manager, parsed_files, workspace, &binary_packages, compile_options)?;

    let contract_warnings_or_errors: CompilationResult<()> = compile_contracts(
        file_manager,
        parsed_files,
        &contract_packages,
        compile_options,
        &workspace.target_directory_path(),
    )?;

    let result = match (program_warnings_or_errors, contract_warnings_or_errors) {
        (Ok((_, program_warnings)), Ok((_, contract_warnings))) => {
            let warnings = [program_warnings, contract_warnings].concat();
            Ok(((), warnings))
        }
        (Err(program_errors), Err(contract_errors)) => {
            Err([program_errors, contract_errors].concat())
        }
        (Err(errors), _) | (_, Err(errors)) => Err(errors),
    };
    Ok(result)
}

/// Compile the given binary packages in the workspace.
fn compile_programs(
    file_manager: &FileManager,
    parsed_files: &ParsedFiles,
    workspace: &Workspace,
    binary_packages: &[Package],
    compile_options: &CompileOptions,
) -> Result<CompilationResult<()>, CliError> {
    // Load any existing artifact for a given package, _iff_ it was compiled with the same nargo version.
    // The loaded circuit includes backend specific transformations, which might be different from the current target.
    let load_cached_program = |package| {
        let program_artifact_path = workspace.package_build_path(package);
        read_program_from_file(&program_artifact_path)
            .ok()
            .filter(|p| p.noir_version == NOIR_ARTIFACT_VERSION_STRING)
            .map(|p| p.into())
    };

    let compile_package = |package| -> Result<CompilationResult<()>, CliError> {
        let cached_program = load_cached_program(package);

        // Hash over the entire compiled program, including any post-compile transformations.
        // This is used to detect whether `cached_program` is returned by `compile_program`.
        let cached_hash =
            cached_program.as_ref().map(|prog| rustc_hash::FxBuildHasher.hash_one(prog));

        // Compile the program, or use the cached artifacts if it matches.
        match compile_program(
            file_manager,
            parsed_files,
            workspace,
            package,
            compile_options,
            cached_program,
        ) {
            Ok((program, warnings)) => {
                // The program is fully optimized during compilation, so if it matches the cached
                // artifact there is nothing new to save.
                if cached_hash == Some(rustc_hash::FxBuildHasher.hash_one(&program)) {
                    return Ok(Ok(((), warnings)));
                }
                // Check solvability.
                match nargo::ops::check_program(&program) {
                    Ok(()) => {
                        // Overwrite the build artifacts with the final, optimized circuit.
                        let _ = save_program_to_file(
                            &program.into(),
                            &package.name,
                            &workspace.target_directory_path(),
                        )?;
                        Ok(Ok(((), warnings)))
                    }
                    Err(errors_and_warnings) => Ok(Err(errors_and_warnings)),
                }
            }
            Err(errors_and_warnings) => Ok(Err(errors_and_warnings)),
        }
    };

    // Configure a thread pool with a larger stack size to prevent overflowing stack in large programs.
    // Default is 2MB. Limit threads to the number of packages we actually need to compile.
    let num_threads = rayon::current_num_threads().min(binary_packages.len()).max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .stack_size(4 * 1024 * 1024)
        .build()
        .unwrap();
    let program_results = pool.install(|| {
        binary_packages.par_iter().map(compile_package).collect::<Result<Vec<_>, _>>()
    })?;

    // Collate any warnings/errors which were encountered during compilation.
    Ok(collect_errors(program_results).map(|(_, warnings)| ((), warnings)))
}

/// Compile the given contracts in the workspace.
fn compile_contracts(
    file_manager: &FileManager,
    parsed_files: &ParsedFiles,
    contract_packages: &[Package],
    compile_options: &CompileOptions,
    target_dir: &Path,
) -> Result<CompilationResult<()>, CliError> {
    let contract_results = contract_packages
        .par_iter()
        .map(|package| -> Result<CompilationResult<()>, CliError> {
            match compile_contract(file_manager, parsed_files, package, compile_options) {
                Ok((contract, warnings)) => {
                    save_contract(
                        contract,
                        package,
                        target_dir,
                        compile_options.show_artifact_paths,
                    )?;
                    Ok(Ok(((), warnings)))
                }
                Err(errors_and_warnings) => Ok(Err(errors_and_warnings)),
            }
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    // Collate any warnings/errors which were encountered during compilation.
    let errors = collect_errors(contract_results).map(|(_, warnings)| ((), warnings));
    Ok(errors)
}

fn save_contract(
    contract: CompiledContract,
    package: &Package,
    target_dir: &Path,
    show_artifact_paths: bool,
) -> Result<(), CliError> {
    let contract_name = contract.name.clone();
    let artifact_path = save_contract_to_file(
        &contract.into(),
        &format!("{}-{}", package.name, contract_name),
        target_dir,
    )?;
    if show_artifact_paths {
        println!("Saved contract artifact to: {}", artifact_path.display());
    }
    Ok(())
}

/// nemi patch: compiles the workspace's contracts in several nargo processes (see `ContractFnSharding` in
/// `noirc_driver`), then merges their artifacts into the ones a single process would write.
///
/// - `NARGO_CONTRACT_FN_JOBS=<n>` (n > 1): compile with n processes. Only for workspaces whose selected packages are
///   all contracts (libraries aside); otherwise ignored.
/// - `NARGO_CONTRACT_FN_CACHE_DIR=<dir>`: the per-function cache, shared by the processes. It also keeps how long each
///   function took (`fn-times.txt`), to start the slowest first next time.
mod sharded {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use nargo::workspace::Workspace;
    use noir_artifact_cli::fs::artifact::save_contract_to_file;
    use noirc_artifacts::contract::ContractArtifact;

    use crate::errors::CliError;

    pub(super) const SHARD_TARGET_ENV: &str = "NARGO_CONTRACT_FN_SHARD_TARGET";
    const JOBS_ENV: &str = "NARGO_CONTRACT_FN_JOBS";
    const CLAIM_DIR_ENV: &str = "NARGO_CONTRACT_FN_CLAIM_DIR";
    const CACHE_DIR_ENV: &str = "NARGO_CONTRACT_FN_CACHE_DIR";
    const ORDER_ENV: &str = "NARGO_CONTRACT_FN_ORDER";
    const TIMES_FILE: &str = "fn-times.txt";

    /// The number of processes to compile with, if this process should start them.
    pub(super) fn jobs(workspace: &Workspace) -> Option<usize> {
        if std::env::var_os(CLAIM_DIR_ENV).is_some() {
            return None; // a worker
        }
        let jobs: usize = std::env::var(JOBS_ENV).ok()?.parse().ok()?;
        let only_contracts = workspace
            .into_iter()
            .filter(|package| !package.is_library())
            .all(|package| package.is_contract());
        (jobs > 1 && only_contracts).then_some(jobs)
    }

    fn generic(message: impl std::fmt::Display) -> CliError {
        CliError::Generic(message.to_string())
    }

    pub(super) fn compile(workspace: &Workspace, jobs: usize) -> Result<(), CliError> {
        let target = workspace.target_directory_path();
        let work = target.join(format!(".shards-{}", std::process::id()));
        let claims = work.join("claims");
        std::fs::create_dir_all(&claims).map_err(generic)?;
        let result = compile_in(jobs, &target, &work, &claims);
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    fn compile_in(
        jobs: usize,
        target: &Path,
        work: &Path,
        claims: &Path,
    ) -> Result<(), CliError> {
        let cache_dir = std::env::var_os(CACHE_DIR_ENV).map(PathBuf::from);
        let times_path = cache_dir.as_ref().map(|dir| dir.join(TIMES_FILE));
        let old_times = times_path.as_deref().map(read_times).unwrap_or_default();

        // the slowest functions first, by their last compile time
        let order = std::env::var(ORDER_ENV).unwrap_or_else(|_| {
            let mut times: Vec<_> = old_times.iter().collect();
            times.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            times.into_iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(",")
        });

        // the workers run this same command, each with its own target directory
        let exe = std::env::current_exe().map_err(generic)?;
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        let mut children = Vec::new();
        for i in 0..jobs {
            let child = Command::new(&exe)
                .args(&args)
                .env_remove(JOBS_ENV)
                .env(CLAIM_DIR_ENV, claims)
                .env(ORDER_ENV, &order)
                .env(SHARD_TARGET_ENV, work.join(format!("target-{i}")))
                .spawn()
                .map_err(generic)?;
            children.push(child);
        }
        let mut failed = false;
        for mut child in children {
            failed |= !child.wait().map_err(generic)?.success();
        }
        if failed {
            return Err(generic("a nargo worker process failed"));
        }

        // merge the partial artifacts of each contract, functions in declaration order
        let claimed = read_claims(claims)?;
        let first = work.join("target-0");
        let mut artifact_names: Vec<_> = std::fs::read_dir(&first)
            .map_err(generic)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        artifact_names.sort();
        for path in artifact_names {
            let file_name = path.file_name().expect("artifact has a file name").to_owned();
            let mut merged: Option<ContractArtifact> = None;
            for i in 0..jobs {
                let part_path = work.join(format!("target-{i}")).join(&file_name);
                let bytes = std::fs::read(&part_path).map_err(generic)?;
                let part: ContractArtifact = serde_json::from_slice(&bytes).map_err(generic)?;
                match &mut merged {
                    None => merged = Some(part),
                    Some(merged) => {
                        merged.functions.extend(part.functions);
                        merged.file_map.extend(part.file_map);
                    }
                }
            }
            let mut merged = merged.expect("at least one worker");
            let contract = merged.name.clone();
            let index = |name: &str| claimed.get(&format!("{contract}-{name}")).map(|c| c.index);
            if merged.functions.iter().any(|f| index(&f.name).is_none()) {
                return Err(generic(format!("unclaimed function in contract {contract}")));
            }
            merged.functions.sort_by_key(|f| index(&f.name));
            let stem = Path::new(&file_name).file_stem().expect("json file").to_string_lossy();
            save_contract_to_file(&merged, &stem, target)?;
        }

        // remember the compile times (a reused function keeps its previous one)
        let mut times = old_times;
        let (mut compiled, mut reused) = (0, 0);
        for (name, claim) in &claimed {
            match claim.time {
                Some((ms, false)) => {
                    times.insert(name.clone(), ms);
                    compiled += 1;
                }
                Some((ms, true)) => {
                    times.entry(name.clone()).or_insert(ms);
                    reused += 1;
                }
                None => {}
            }
        }
        if let Some(times_path) = times_path {
            let contents: String = times.iter().map(|(name, ms)| format!("{name} {ms}\n")).collect();
            let _ = std::fs::create_dir_all(times_path.parent().expect("has a parent"));
            let _ = std::fs::write(times_path, contents);
        }
        eprintln!(
            "Compiled {compiled} contract functions in {jobs} processes, reused {reused} unchanged ones"
        );
        Ok(())
    }

    struct Claim {
        index: usize,
        /// compile time in ms, and whether the function was reused from the cache
        time: Option<(u64, bool)>,
    }

    /// The claim files: `<contract>-<function>` holds `<declaration index>`, then ` <ms> <compiled|cached>` once done.
    fn read_claims(dir: &Path) -> Result<BTreeMap<String, Claim>, CliError> {
        let mut claims = BTreeMap::new();
        for entry in std::fs::read_dir(dir).map_err(generic)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let contents = std::fs::read_to_string(entry.path()).map_err(generic)?;
            let mut parts = contents.split_whitespace();
            let index = parts.next().and_then(|p| p.parse().ok()).unwrap_or(usize::MAX);
            let time = match (parts.next().and_then(|p| p.parse().ok()), parts.next()) {
                (Some(ms), Some(how)) => Some((ms, how == "cached")),
                _ => None,
            };
            claims.insert(name, Claim { index, time });
        }
        Ok(claims)
    }

    fn read_times(path: &Path) -> BTreeMap<String, u64> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (name, ms) = line.rsplit_once(' ')?;
                Some((name.to_string(), ms.parse().ok()?))
            })
            .collect()
    }
}
